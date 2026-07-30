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

Current-evidence boundary: 282 Rust tests are discovered: 281 regular tests pass
and 1 authenticated live test is ignored by default. The ignored
stream/resume test was also run explicitly against the installed Copilot CLI
and passed. PTY-16 through PTY-28 are current compiled-binary evidence; earlier
PTY references remain useful historical records.

## Findings remediated during this audit

| Priority | Finding | Resolution and evidence |
|---|---|---|
| P0 | Split view paired deletions/additions into fewer rows than the reducer used, so cursor and persisted anchors could diverge. | Split rendering is now one canonical parsed row per display row. `WF::file_switch_clears_visual_mode_and_layout_changes_preserve_selection`. |
| P0 | Placements could not distinguish old-file and new-file line numbers. | Migration 2 adds placement `side`; deleted selections persist as `old`, new/context selections as `new`. `WF::deleted_line_annotation_records_the_old_side`. |
| P0 | Fold/meta selections could persist as line 0; replacement selections could mix incompatible old/new coordinates. | Annotation validation rejects metadata and mixed-side ranges with an explanatory status. `WF::folds_and_mixed_old_new_ranges_fail_safely`. |
| P0 | Local Ask/Comment always failed while pinning the first snapshot because `:` from the version ID was placed in a Git ref. | Snapshot refs use a deterministic Git-safe SHA-256 name. `git::tests::snapshot_preserves_head_and_index_and_pins_the_worktree`, PTY-04. |
| P0 | Compiled-terminal Enter/control sequences could leave composers and the command palette impossible to submit. | Enter plus terminal-equivalent `Ctrl-J`/`Ctrl-M` are accepted; `Ctrl-S` remains supported. `app::tests::terminal_ctrl_j_equivalent_submits_composers_and_commands`, PTY-04/05/08. |
| P0 | The public headless harness captured effect strings but did not execute effects. | It now uses a temporary SQLite database, fake agent, full effect runner, event injection, restart, persisted-row inspection, and styled-frame inspection. All `WF::*` tests use it. |
| P0 | Persistence errors discarded the composer draft and could terminate the interaction loop. | Effect failures are contained; contextual mode, range, target, and draft are restored. `WF::failed_persistence_restores_the_contextual_composer_and_draft`. |
| P0 | A closed agent command channel could restore an already-persisted Ask composer, allowing a duplicate on retry. | The single Ask remains visibly failed and pending for explicit recovery; the composer is not recreated. `WF::closed_agent_channel_keeps_one_pending_ask_without_duplicate_retry`. |
| P0 | Some terminal setup failures could skip raw/alternate-screen restoration. | Setup and all run-loop exits now execute cleanup. PTY-10/11 prove normal and forced exits. |
| P0 | `review` could try to initialize storage, remote resolution, the Copilot worker, and raw mode when stdin/stdout were redirected. | The CLI now rejects a non-interactive review before any of those operations. `cli::tests::review_rejects_non_interactive_terminal_before_tui_startup` and the Bazel-built `non_tty_cli_test` verify the message and absence of escape bytes. |
| P1 | Visual mode was cleared or left stale inconsistently across search, cancel, file/screen/layout changes. | Search returns to Visual and extends its fixed anchor; cancel restores the exact semantic mode/range; file and screen changes clear predictably while split/unified layout changes preserve and reproject the selection. `WF::review_visual_cancel_restores_the_exact_character_range`, `WF::visual_search_extends_the_fixed_anchor_and_chat_visual_yanks_messages`, `WF::file_switch_clears_visual_mode_and_layout_changes_preserve_selection`. |
| P1 | Split mode highlighted both copies of context or only the cursor, rather than the selection's valid source side. | Every selected canonical row now styles only its annotation-valid old/new cell; context defaults to new, context plus deletion resolves old, and the opposite pane stays unpainted. `WF::split_visual_selection_paints_only_the_annotation_side`. |
| P1 | Review Visual was whole-line only, so `v`, `V`, and `Ctrl-V` were indistinguishable and long-line endpoints were unreachable. | Review now has semantic character, line, and block modes; grapheme/cell-aware `h/l/0/$/w/b`; exact mode-specific copy; full-line annotation projection; horizontal source viewport markers; and explicit mode, side, row, column, and viewport feedback. `WF::review_visual_modes_have_distinct_rendering_and_exact_copy_semantics`, `WF::review_character_selection_moves_and_copies_extended_graphemes_atomically`, `WF::long_review_selection_pans_to_semantic_end_and_back`, PTY-27. |
| P1 | Visual `y` was unreachable as a one-key action. | Visual `y` executes immediately; `yy` remains Normal-mode yank. `WF::visual_yank_preserves_source_order_across_addition_and_context`. |
| P1 | `Ctrl-w h/j/k/l` required Control on the second key, contrary to the documented sequence. | A plain second direction is accepted. `app::tests::ctrl_w_then_plain_direction_moves_focus_as_documented`. |
| P1 | File-tree and annotation navigation exposed obsolete third-pane behavior. | `t` is the sole direct file-tree toggle, Enter accepts the current file, and the annotation rail/focus target is removed. Both diff layouts render annotations as full-width inline blocks. |
| P1 | Command entry and annotation composition were confined to the bottom status line. | Commands render in a top palette; annotation composers render beside the selected range and remain usable in narrow terminals. `WF::narrow_terminal_keeps_selection_composer_and_top_palette_visible`, PTY-03/08. |
| P1 | Export could retain an unrelated previous status after successfully queueing a batch. | Export now always reports the queued batch and optional written path. `WF::comment_export_queues_one_batch_and_acknowledges_delivery`. |
| P1 | Narrow command/Ask overlays leaked fragments of underlying panes, making their borders and content ambiguous. | Compact terminals use full-width cleared modal surfaces; the 40×12 adversarial reproductions are clean. |
| P1 | Review focus and advertised `Ctrl-W` arrow chords were not visibly reliable. | The header and focused pane titles show focus, arrows and letter chords share one path, unavailable destinations report an explicit status, Chat is excluded from the two-window ring, and incomplete chords visibly expire after 1.5 seconds. `app::tests::ctrl_w_then_plain_direction_moves_focus_as_documented`. |
| P1 | Wide CJK/emoji text wrapped by character count and clipped terminal cells. | Markdown wrapping now uses Ratatui's terminal-cell width, and snapshot extraction omits wide-character continuation cells. `chat_render::tests::wide_unicode_wraps_by_terminal_cells_without_clipping`. |
| P1 | Lagged/closed SDK subscriptions, stale deltas, and resumed pending work could strand or corrupt the local active turn. | Lagged/closed streams fail active and queued work visibly instead of hanging; resumed in-flight turns receive a synthetic local outbound; a dispatched prompt binds through its matching `user.message` event before descendants are accepted, so late events from an older chain cannot complete the new turn. Corresponding `copilot::tests::*` race tests and LIVE-01 pass. |
| P1 | SDK control calls could wait forever without a diagnosable operation boundary. | Startup, session create/resume, history, model, steering, delivery, fork, compaction, abort, disconnect, deletion, and shutdown calls now carry named 15-second async timeouts. The SDK worker remains on its own OS thread so even a CLI-side synchronous startup stall cannot block input or rendering. |
| P1 | Model preferences were global despite sessions being scoped to Work Items, compact picker rows hid model identity, and SIDE could appear to mutate MAIN implicitly. | Model/reasoning/context preferences now load and persist per Work Item with global defaults for new items; compact rows retain both display name and model ID; `:model` is explicitly MAIN-scoped and blocked with guidance while SIDE is active. `ui::tests::model_preferences_are_scoped_to_each_work_item` and `WF::model_picker_is_explicitly_main_scoped_while_side_is_active`. |
| P1 | A resumed session whose history reload failed could immediately overwrite the only warning with a generic ready status. | Session readiness now carries the history warning as typed state and leaves it visible while keeping the resumed session usable. `ui::tests::resumed_session_history_failure_remains_a_visible_warning`. |
| P1 | Immediate steering was persisted as a queued correction but acknowledged only against the active root turn, leaving a phantom pending item after completion, abort, or restart. | Steering now has correlated typed accepted/failed events. Immediate acknowledgement settles its own durable row and pending ID; a race that finds no active turn remains an ordinary visible queued request. Accepted, rejected, and restart behavior are deterministic. |
| P1 | `Ctrl-C` entered STOPPING immediately, while `:stop`, `:abort`, and `s` in Queue/Agent Status could continue displaying RESPONDING; delivery failure or an abort/idle race could strand STOPPING. | Every stop surface shares one reducer transition, including `Ctrl-C` inside Queue and Agent Status. Duplicate and idle stops are explicit, delivery failure returns to retryable RESPONDING, and a typed already-idle acknowledgement settles a raced stop. `app::tests::queue_and_agent_status_stop_controls_share_the_same_transition`; `ui::tests::abort_delivery_failure_never_leaves_a_false_stopping_state`; `ui::tests::stop_acknowledgement_after_sdk_idle_cannot_leave_the_ui_stopping`. |
| P1 | SDK session-terminal events can legitimately omit `parent_id`; rejecting them as uncorrelated could leave a completed or failed turn permanently active. | Unparented `session.idle`, `session.error`, and `session.task_complete` are accepted as session boundaries while parented events from stale turn chains remain quarantined. Raw-event tests cover completion, failure, task completion, and stale-chain rejection. |
| P1 | Delayed steering acknowledgements could settle their correction but rewind progress from a newer turn to the older turn. | Steering accepted/failed events always settle their own durable record, but mutate active phase/outbound only when their correlated turn is still current; late results append an observation instead. `ui::tests::late_steering_results_settle_without_rewinding_the_current_turn`. |
| P2 | A cancelled controlled turn left the next queued test turn scheduled behind the old completion deadline. | The generic deterministic agent now advances the next queued turn immediately after abort while retaining only the first turn's typed cancellation markers. `copilot::tests::controlled_abort_advances_the_next_queued_turn_immediately`. |
| P1 | The checked-in Bzlmod lock was incomplete for strict consumers and the root alias could not analyze under `bazel test //...`. | The lockfile is refreshed, CI/release use `--lockfile_mode=error`, package visibility admits only the root alias, and the full aggregate Bazel gate passes. |
| P1 | Opening an older reviewed remote version could delete a newer unseen version, and carry-forward could flip an old-side annotation onto the new side. | Remote cleanup now targets only lower version numbers, and re-anchoring preserves the prior placement side while reading the matching base/current source. `storage::tests::unopened_remote_cleanup_never_targets_a_newer_version`; `annotations::tests::exact_reanchor_preserves_the_placement_side`. |
| P1 | Delivered annotation corrections were sent without durable recovery metadata. | Migration 6 records outbox kind and MAIN/SIDE lane; corrections are persisted before delivery and require explicit resend/discard after restart. `WF::delivered_annotation_correction_requires_explicit_recovery_after_restart`. |
| P1 | The 40×9 contextual composer lost its rectangle or clipped the insertion cursor, and compact file/version lists could hide the selected row. | Compact editors now retain top/body/bottom borders with an independently scrolled content viewport; file and version lists window around their selection. The exact-minimum composer, file-tree, and version-history tests cover these states. |
| P1 | Bracketed multiline paste was dropped or submitted only its first line; generic Vim prefixes could remain invisibly armed; queue editing looked like a new prompt. | Bracketed paste is enabled and inserted atomically with visible multiline feedback and an exact visible-row range even at 40×9; every prefix is labelled, cancellable, and timed; queue replacement uses a yellow `EDIT QUEUED <id>` contract. PTY-26 and the corresponding deterministic paste/prefix/queue tests pass. |
| P2 | Long source rows were silently clipped, blocked-quit warnings could be overwritten by SDK progress, and cancellation before the first response delta left no transcript marker. | Both diff layouts place a styled `…` in the final visible cell of clipped rows; quit guards render from dedicated state until dismissed/forced; early abort marks the submitted turn as cancelled. The long-line, quit-guard, and early-cancel deterministic workflows cover these cases. |

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
| R-10 | Version picker with exact per-version counts and old-version reopening | Partially working | Storage queries and a selection-pinned compact screen exist; opening a reviewed version can prune only older unseen versions, never newer history. Full old-version materialization still lacks an external workflow test. |
| R-11 | Annotation identity plus per-version placement | Working end-to-end | Temporary SQLite workflows persist and reload exact placements; migration/storage tests pass. |
| R-12 | Content snippet/hash anchoring and fuzzy carry-forward | Partially working | Exact, ambiguous, missing-anchor, moved-content, and old/new-side-preservation tests pass; full remote-version carry-forward remains untested. |
| R-13 | Rename following and `≈` ambiguity review | Partially working | Rename parsing and re-anchor primitives exist; no complete rename-fetch workflow. |
| R-14 | Normal/Visual Ask grounded to exact range, queued serially, streamed inline | Working end-to-end | `WF::normal_line_ask_persists_queues_streams_and_completes_inline`; `WF::visual_range_ask_uses_exact_new_source_range`; PTY-05/06/07. |
| R-15 | Inline Ask follow-ups remain self-contained | Working end-to-end | `WF::annotation_edit_delete_undo_and_inline_follow_up_are_complete_workflows`; message envelope test coverage in `annotations.rs`. |
| R-16 | Comment persists locally and does not contact Copilot until export | Working end-to-end | `WF::normal_and_visual_comments_persist_without_contacting_agent`; PTY-04. |
| R-17 | `:fork` child session becomes active | Partially working | Command, queued control sequencing, SDK call, and storage handling exist; no authenticated fork smoke test. |
| R-18 | Structured local context generation, editing, acceptance, persistence | Partially working | Parser/editor/effects and controlled-agent shape exist; no complete attach/restart workflow test. |
| R-19 | Split and unified review layouts with inline annotations and no rail | Working in deterministic tests | Unified is the default. Both layouts retain one diff cursor, preserve semantic selection across layout changes, and render Comment/Ask content inline; split blocks span both columns while source selection paints only its valid side. `WF::file_switch_clears_visual_mode_and_layout_changes_preserve_selection`, `WF::split_visual_selection_paints_only_the_annotation_side`, and `WF::normal_and_visual_comments_persist_without_contacting_agent`. Navigation through individual block rows remains a follow-up. |
| R-20 | Multi-repo grouped file/folder picker ordered by activity | Partially working | Repo grouping, collapse/expand, fuzzy filtering, and Enter work; folder hierarchy and complete activity-order workflow are absent. |
| R-21 | Full Chat panel with streaming, queue/activity/usage, model/session controls | Working end-to-end for the audited Chat sequence | Streaming, durable queued-chat recovery, activity, usage, rendered-row scrolling, sticky composer, and character/line/block selection are deterministic; PTY-24 additionally proves exact character/line clipboard bytes. Model capability picking, steering, quiet detection, active cancellation, and queued cancellation are covered by deterministic `WF::*` tests plus the scoped PTY-16–25 evidence. Optional SDK surfaces remain classified separately in `copilot-sdk-audit.md`. |
| R-22 | Top command palette with autocomplete and execution | Working end-to-end | `WF::narrow_terminal_keeps_selection_composer_and_top_palette_visible`; PTY-08. |
| R-23 | Settings screen | Partially working | Model, base, and context-step edits work. Diff-layout, file-tree, and Markdown launch defaults persist in SQLite; cache, skills, and storage paths are visible; compact viewports keep the selected row visible. Keybinding viewing/editing and detailed `gh auth` identity remain incomplete. |
| R-24 | Generated context editor six-field presentation | Partially working | Parser and editor render; complete accept-to-session workflow untested. |
| R-25 | Prune reviewed history, optional export, git/cache cleanup | Partially working | Review-scoped selection, visibly disabled current item, scrollable narrow rendering, typed mixed results, nonblocking controlled execution, startup recovery, durable export intent, collision-resistant archive names, per-Work-Item process locking, owner-fenced Git/filesystem cleanup, and atomic retry-safe final deletion are covered by deterministic tests and PTY-22. PR-state/size enrichment remains incomplete. |
| R-26 | Markdown/JSON comment export and one structured session batch | Working end-to-end | Export format tests and `WF::comment_export_queues_one_batch_and_acknowledges_delivery`. |
| R-27 | Never silently resend; pending before send, sent atomically at response start, explicit restart recovery | Working end-to-end | `storage::tests::ask_delivery_ack_is_persisted_atomically_with_response_start`; `WF::failed_agent_delivery_is_pending_and_restart_requires_recovery_choice`. |
| R-28 | WAL, busy timeout, FKs, migrations, indexed SQLite model | Working end-to-end | Storage migration/concurrency tests; workflow tests use a real temporary SQLite file. |
| R-29 | Editing delivered content queues correction rather than rewriting history | Working in deterministic persistence/recovery tests | Effects persist correction kind/lane before SDK delivery and clear only at response start; delivered annotation edits recover explicitly after restart. A real authenticated correction turn is not part of the regular gate. |
| R-30 | Lazy viewport syntax highlighting behind a trait, broad language support | Working end-to-end | `highlight::tests::recognizes_common_languages`; `highlight::tests::highlights_only_requested_lines_and_reuses_cache`; deterministic rendered frames. |
| R-31 | Read/search-only Ask permissions | Working in the audited local Copilot path | `copilot::tests::permission_handler_allows_reads_and_denies_shell_and_write`; LIVE-01 ran the authenticated bridge with its configured read-only permission handler. |
| R-32 | Streaming events and persisted-session resume | Working end-to-end | Deterministic delta/final/resync/sub-agent tests pass; LIVE-01 streamed a real response, exercised SIDE teardown, disconnected, resumed the persisted MAIN session, and reloaded history. |
| R-33 | Session deletion during prune or manual storage-path guidance | Working in deterministic tests | The SDK adapter deletes/absence-checks every persisted MAIN/fork and parent-bound SIDE target before local deletion, pre-journals client-selected MAIN IDs, durably records fork creation intents, atomically promotes a fork while clearing its intent, captures rejected late activations, records targets in a cascade-independent SQLite journal, conditionally advances that journal only for its lease owner, resumes interrupted operations, and reports `$COPILOT_HOME/session-state/<id>` (or the platform fallback) on failure. Live destructive SDK deletion is deliberately not exercised. |
| R-34 | Interdiff view | Explicitly deferred by the specification | v1.x item in §11. |
| R-35 | Live file-watcher/re-anchor cadence while review remains open | Explicitly deferred by the specification | Open item in §11; manual `:sync` is present. |
| R-36 | Exact fuzzy threshold tuning and eager/lazy snapshot worktree policy | Explicitly deferred by the specification | Open items in §11. |

## Visual-mode traceability

| ID | Required Visual behavior | Classification | Evidence |
|---|---|---|---|
| V-01 | `v`, `V`, and `Ctrl-V` enter character, line, and block Visual modes in diff | Working end-to-end | `WF::review_visual_modes_have_distinct_rendering_and_exact_copy_semantics`; PTY-02/27. |
| V-02 | Visible Visual mode, side, rows, columns, and horizontal viewport | Working end-to-end | `WF::review_visual_modes_have_distinct_rendering_and_exact_copy_semantics`, `WF::long_review_selection_pans_to_semantic_end_and_back`; PTY-02/27. |
| V-03 | Fixed anchor while `j/k`, page, and search movement extend | Working end-to-end | `WF::visual_search_extends_the_fixed_anchor_and_chat_visual_yanks_messages`; range workflows exercise `j`. |
| V-04 | Every selected row has restrained visible styling | Working end-to-end | Styled-cell assertions in split/unified workflow test. |
| V-05 | Selection understandable in split and unified | Working end-to-end | Layout switches preserve semantic selection, and split context paints only the valid source side. `WF::file_switch_clears_visual_mode_and_layout_changes_preserve_selection`; `WF::split_visual_selection_paints_only_the_annotation_side`. |
| V-06 | Fold/meta rows cannot become invalid anchors | Working end-to-end | `WF::folds_and_mixed_old_new_ranges_fail_safely`. |
| V-07 | `a` opens exact-range contextual Ask composer | Working end-to-end | `WF::visual_range_ask_uses_exact_new_source_range`; PTY-03 demonstrates contextual placement for the sibling composer. |
| V-08 | `c` opens exact-range contextual Comment composer | Working end-to-end | Comment workflow; PTY-03. |
| V-09 | Submission persists source lines, never viewport rows | Working end-to-end | New/old placement assertions across multiple non-1-based hunks. |
| V-10 | Annotation appears immediately at its anchor | Working end-to-end | Unified workflow frames and PTY-04. |
| V-11 | Ask queues and streams inline | Working end-to-end | Ask workflows and PTY-05/06. |
| V-12 | Comment remains local | Working end-to-end | Comment workflow checks zero agent commands; PTY-04. |
| V-13 | `y` copies exact character/block source text or complete newline-terminated selected lines | Working end-to-end | `WF::review_visual_modes_have_distinct_rendering_and_exact_copy_semantics`, Unicode and long-line workflows, and `WF::visual_yank_preserves_source_order_across_addition_and_context`. |
| V-14 | `Esc` clears Visual | Working end-to-end | File-switch/cancel workflows. |
| V-15 | File/repo/version/screen/layout transitions clear or preserve predictably | Working end-to-end for file/screen/layout; partially working for version | File/screen/layout tests pass. Old-version switching lacks full workflow coverage. |
| V-16 | Normal `a`/`c` uses current code line | Working end-to-end | Normal Ask and Comment workflows. |
| V-17 | Binary/empty/deleted/renamed/fold cases fail safely | Working end-to-end | `WF::binary_empty_and_renamed_files_have_explicit_safe_behavior`, deleted-line workflow, and fold/mixed-side workflow. |
| V-18 | Chat Visual selects visible meaningful text | Working in deterministic tests | `v`, `V`, and `Ctrl-V` select mapped rendered text by character, line, and block. Source-based endpoints survive Markdown wrapping, resize/reflow, Unicode, fenced code, and streaming updates; `WF::chat_semantic_modes_map_markdown_code_and_unicode_without_message_wide_highlighting` and `WF::chat_page_keys_extend_semantic_selection_across_wrap_resize_and_streaming`. Compiled-terminal evidence remains to be recaptured. |
| V-19 | Footer exposes Visual actions | Working end-to-end | Frame assertions and PTY-02. |
| V-20 | Composer cancellation restores the exact prior mode/range without stale draft | Working end-to-end | `WF::review_visual_cancel_restores_the_exact_character_range`, `WF::cancel_restores_visual_selection_without_stale_draft`. |

Source-line policy established by the audit:

- The reducer stores canonical parsed-diff rows plus semantic
  character/block terminal-cell columns; syntax spans and gutters are never
  selection coordinates.
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
| `gm` | Partially working | Inline Markdown generation, scrolling, persisted inline/browser default selection, and an in-overlay browser-fallback action are tested. The external browser process launch is not exercised by the deterministic suite. |
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
| `:prune` | Partially working | Review-scoped selection and deterministic destructive workflows cover current-item rejection, SDK-first session cleanup, durable restart retry, partial batches, and local cleanup. PR-state/size enrichment and authenticated destructive SDK deletion remain untested. |
| `:settings` | Partially working | Model/base/context editing and persisted diff-layout, file-tree, and Markdown defaults are tested. Keybinding editing and detailed `gh auth` identity remain incomplete. |
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
- Authenticated destructive Copilot session deletion is intentionally untested;
  the SDK deletion, absence verification, durable retry journal, and manual
  storage-path guidance are covered through the generic cleanup adapter.
- Settings keybinding editing, detailed `gh auth` identity, and folder
  hierarchy in the picker are incomplete.
- Interdiff and live file-watcher policy remain explicitly deferred by v2.

## Current TUI/chat UX addendum

This addendum records the implementation that was added after the original
diff/annotation audit. It is intentionally separate from the older requirement
matrix so that new evidence is not confused with the earlier PTY run.

| ID | Behavior | Current classification | Evidence / limitation |
|---|---|---|---|
| T-01 | Chat has explicit NORMAL, INSERT, COMMAND, SEARCH, and VISUAL modes with visible mode text | Working in deterministic frames and compiled PTY | `testing::tests::command_palette_is_scrollable_selectable_and_keeps_the_sticky_composer`, `testing::tests::sticky_chat_composer_wraps_edits_preserves_and_explicitly_discards_drafts`, PTY-23, and the `ui-snapshot` gallery. |
| T-02 | Command palette selection and scrolling | Working in the deterministic harness and compiled PTY | Composer-local completions retain the sticky input and preserved draft; Up/Down wrap, PageUp/PageDown, Home/End, Tab completion, mouse wheel routing, narrow text-tail scrolling, and visible selection are covered by deterministic tests plus PTY-12/23. |
| T-03 | Chat scrolling by rendered rows, including wrapped single messages | Working in the deterministic harness and compiled PTY | `WF::chat_scrolls_by_rendered_rows_and_pauses_live_following`; PTY-24 verifies immediate normal-mode Up/PageUp/SGR-wheel movement and resize reflow against visible row ranges. Native mouse-drag remains a terminal-owned fallback rather than app state. |
| T-04 | Sticky multiline composer with wrapping, independent scroll, editing, preserved drafts, explicit discard, and atomic multiline paste | Working in the deterministic harness and compiled PTY | `WF::sticky_chat_composer_wraps_edits_preserves_and_explicitly_discards_drafts`; exact-minimum tests keep the full rectangle and cursor intact; PTY-21 exercised a 21-row contextual composer, PTY-23 exercised the exact 40×9 Chat composer, and PTY-26 proves bracketed two-line paste remains one editable draft. |
| T-05 | Cancel draft versus cancel active Copilot turn | Working in the deterministic harness and compiled PTY | Draft preservation/discard and deterministic abort/reusable-session paths pass. `Ctrl-C`, `:stop`, `:abort`, Queue `s`, and Agent Status `s` enter the same immediate STOPPING state; failed command delivery becomes actionable rather than pretending cancellation is underway. PTY-18 stopped an active response while preserving the draft in Insert mode. |
| T-06 | Markdown semantics in Chat and fenced-code rendering | Working in deterministic frames | `chat_render::*`, `markdown::*`, and `WF::markdown_history_and_minimum_terminal_state_have_inspectable_frames`; the source-mapped terminal renderer covers headings, lists, task lists, nested blockquotes, emphasis, inline code, safe link affordances, pipe tables, wrapping, blank lines, and syntax-highlighted fences. Browser output shares safe inline parsing, rejects executable URL schemes, and renders aligned tables. |
| T-07 | Lazy cached syntax highlighting for diff lines and fenced code | Working in unit tests and deterministic frames | `highlight::tests::recognizes_common_languages`, `highlight::tests::highlights_only_requested_lines_and_reuses_cache`, and `chat_render::tests::highlights_fenced_code_with_a_language_specific_synthetic_path`. |
| T-08 | Durable Copilot progress, quiet warning, tool/skill visibility, and `:agent-status` timeline | Working in the deterministic harness and compiled PTY | The renderer and sticky-footer overlay expose lane, phase, elapsed time, last event, queue, outbound ID, operation detail, quiet diagnostics, and a positioned `X-Y/T` timeline. Keyboard/page/document/mouse scrolling and `s`/`Ctrl-C` cancellation remain visible in compact layouts. Tool, skill, subagent, hook, retry, and error activity are typed and visible. PTY-17 captures steering; PTY-25 captures skill/subagent/retry history, quiet state, cancellation, and immediate FIFO continuation. |
| T-09 | `/side` isolated ephemeral conversation and `/main` restoration | Working in deterministic, compiled-PTY, and authenticated paths | `WF::side_conversation_is_visibly_isolated_and_main_transcript_is_restored`; PTY-20 returned from an active SIDE turn promptly, PTY-28 reproduces the full SIDE/tool/MAIN lifecycle in every Bazel PTY run, and LIVE-01 exercised real SIDE creation and SDK deletion. |
| T-10 | Deterministic state gallery for visual inspection | Working as headless command paths | `ui-snapshot --state all` exercises review, ask, command, composer, quiet, queue, side, model, markdown, and tiny states through the production renderer. `ui-script` drives resize, key input, multiline paste, mouse wheel input, stream events, appendable history, and snapshots. Harness time is frozen so independent runs are byte-identical. Gallery output is terminal text, not a committed image artifact. |
| T-11 | Copilot SDK decoupled behind a testable agent interface | Working for deterministic tests | `TuiHarness` uses a fake agent and injected lane/activity events; the production bridge remains the only path that starts the real Copilot CLI. |
| T-12 | Mouse wheel scrolling and exact terminal text selection | Working in deterministic tests and compiled PTY | Wheel events route through the active mode, so they scroll Chat/diff, command completions, and the multiline composer rather than a hidden underlying surface. Built-in Chat Visual mode selects exact mapped rendered text by character, line, or block and copies source text without Markdown decoration or soft-wrap newlines; cross-message copies include speaker labels. PTY-24 decodes the OSC 52 payload and verifies exact inclusive bytes after keyboard selection; Shift-drag remains available for native terminal selection. |

### Verification status for this addendum

The current checked-in regular-test baseline is:

```text
282 total Rust tests; 281 passed; 1 authenticated live Copilot test ignored by default
```

The regular baseline includes the reducer/effect, storage, rendering, SDK
adapter, deterministic UI, model-picker, queue/steering, liveness, and
MAIN/SIDE tests. The ignored test is the authenticated
`copilot::tests::live_copilot_streams_and_resumes_persisted_history`; it is not
part of the regular count. It was run separately with
`RQ_TUI_LIVE_COPILOT=1` and passed as LIVE-01. PTY-16 through PTY-28 were
captured from the current Bazel-built binary, with PTY-22/23 added by the
automated compiled-binary smoke test and PTY-24/25/26 extending that same test.

### Compiled PTY evidence

Earlier records are kept for reproducibility and design context. The current
pass is PTY-16 through PTY-28 in [`audit/pty-smoke.md`](audit/pty-smoke.md).

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
scrolling. PTY-25 adds wall-clock quiet-warning, stopping, cancellation, and
post-deadline quiescence evidence. PTY-26 adds a real terminal bracketed-paste
sequence containing a literal newline.

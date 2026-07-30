# Compiled-binary pseudo-terminal record

Date: 2026-07-29  
Platform: macOS arm64  
Terminal: tmux 140×35 pseudo-terminal  
Binary: `bazel-bin/src/rq-tui`, produced by `bazel build //:rq-tui`

The run used the opt-in deterministic agent so queue and streaming phases were
controllable without changing the production Copilot path:

```sh
RQ_TUI_CONTROLLED_AGENT=1 \
RQ_TUI_DATA_DIR=/tmp/rq-tui-audit5-data \
RQ_TUI_CACHE_DIR=/tmp/rq-tui-audit5-cache \
bazel-bin/src/rq-tui review . --base main
```

## Recorded scenarios

| ID | Keys/action | Captured terminal evidence |
|---|---|---|
| PTY-01 | Open local review | Header showed `rq-tui — Review`, the current repository/file, split old/new panes, annotation rail, and `Copilot session connected`. |
| PTY-02 | `v j j` | Footer changed to `VISUAL rows 1-2 · a ask · c comment · y yank · Esc clear`; all selected rows carried the selection marker/background in both split panes. |
| PTY-03 | `c`, type comment | Contextual overlay rendered `Comment`, the draft, and `new lines 14-16`. |
| PTY-04 | `Ctrl-J` submit | Rail rendered `▸ [c] ln 14-16 PTY visual comment`; footer reported `Comment saved locally · pinned s1`. No agent turn was created. |
| PTY-05 | Normal-line `a`, type, `Ctrl-J` | Rail rendered `▸ [a] ln 16-16 user: PTY ask question`; footer reported `Agent message queued at position 1`. |
| PTY-06 | Controlled first stream frame | Rail changed to `assistant: Controlled`; footer reported `Copilot is responding…`. |
| PTY-07 | Stream completion and `Tab` | Chat showed `PTY ask question` followed by `Controlled streamed response.` with `agent: ●`. |
| PTY-08 | `:diff unified` before execution | A top overlay rendered `Command palette`, `:diff unified█`, and the matching suggestion. `Ctrl-J` executed it. |
| PTY-09 | `:q` with local comment | App stayed open and showed `Unsubmitted comments/asks exist; use :q! to force quit`. |
| PTY-10 | `:q!` | tmux reported the pane/session exited (`FORCE_QUIT_OK`), demonstrating alternate-screen/process cleanup. |
| PTY-11 | Fresh process, `:q` | Clean review exited normally (`CLEAN_QUIT_OK`). |

## Defects found only through the pseudo-terminal

1. Local annotation submission originally failed because snapshot database IDs
   contain `:`, which is illegal in Git ref names. Snapshot refs now use a
   deterministic SHA-256 encoding. PTY-04 proves the repaired local path.
2. tmux exposed Enter as terminal-equivalent control sequences that the
   synthetic key tests did not model. Composers, command execution, search,
   and normal Enter actions now accept `Ctrl-J`/`Ctrl-M` equivalents. PTY-04,
   PTY-05, PTY-08, PTY-09, and PTY-11 prove those paths.

The tmux sessions used for this record were terminated after the checks.

## Second compiled-binary UX pass

Date: 2026-07-29  
Platform: macOS arm64  
Terminal: tmux 100×28 pseudo-terminal  
Binary: `bazel-bin/src/rq-tui`, produced by `bazel build //:rq-tui`  
Agent: `RQ_TUI_CONTROLLED_AGENT=1`

This pass focused on the newer Chat, command-palette, progress, and SIDE
states. It used the same reducer/effect path as the production binary and was
performed with production data disabled.

| ID | Keys/action | Captured terminal evidence |
|---|---|---|
| PTY-12 | Enter Chat, open `:`, press Down repeatedly | The palette stayed visibly modal, the highlighted suggestion moved down, the visible list scrolled, the count reached `13/20`, and the sticky input bar said `COMMAND MODE ACTIVE`. |
| PTY-13 | Submit `/side Explain the liveness contract briefly` | Chat changed to a clearly labelled `SIDE` lane. The SIDE progress line showed phase, elapsed time, last SDK event age, event count, and queue depth; tool activity was visible below it. |
| PTY-14 | Open `:agent-status` in SIDE | The overlay showed lane, connected state, phase, elapsed time, last SDK event, event count, queue depth, current operation/detail, outbound ID, and a recent activity timeline. `j/k` reached the footer controls. |
| PTY-15 | Submit `/main`, then type a long multiline draft | MAIN returned without the SIDE transcript. The sticky bordered composer wrapped over multiple rows, retained its insertion cursor, and remained separate from the transcript. |

The pass did not produce a reliable capture for the quiet-warning threshold or
active-turn cancellation. The deterministic liveness test is also currently
red; see the verification addendum in
[`../behavioral-audit.md`](../behavioral-audit.md). These are therefore not
claimed as completed PTY behaviors.

# GitHub Copilot SDK audit

Audit date: 2026-07-30

This is a source-level audit of `rq-tui` against the 21 Copilot SDK pages
requested for this project. It is intentionally separate from runtime
validation; current authenticated and PTY evidence is recorded in the linked
behavioral and PTY audit files.

## SDK baseline and parity

The repository uses `github-copilot-sdk` **1.0.8**. The audited upstream source
is commit `a54b0b5885534ba5ad073cfe99c7f0085b1be11c`.

The Rust source exposes the same important protocol surfaces needed by this
application as the current Go SDK: session creation/resume, streamed events,
`Enqueue`/`Immediate` delivery, abort, model listing and switching, hooks,
skills, plugins, MCP, cloud/remote sessions, image attachments, fleet mode,
custom agents, and session limits. No Rust-versus-Go parity blocker was found
for the audited feature set. The limitation is application wiring: `rq-tui`
deliberately uses only the local read-only subset today, and the matrix below
records every unused surface instead of implying that the SDK lacks it.

Status meanings:

- **Implemented** — the current application source wires the feature into the
  production bridge or UI-facing test seam.
- **Partial** — an important subset is wired, with a concrete correctness or
  persistence boundary still recorded in the matrix.
- **Intentionally optional/out-of-scope** — the Rust SDK supports it, but it is
  not part of the local code-review TUI product boundary.
- **Gap** — the Rust SDK supports it and the current application does not yet
  expose or configure it.

## 21-page feature matrix

| Official documentation | Status | Current implementation / boundary |
|---|---|---|
| [Agent loop](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/agent-loop) | **Implemented** | The bridge owns a persistent session, subscribes to events, starts turns asynchronously, and keeps tool-loop progress visible. `session.task_complete` is surfaced as activity; `session.idle` only completes a turn after `backgroundTasks` reports no remaining work. |
| [Cloud sessions](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/cloud-sessions) | **Intentionally optional/out-of-scope** | This product reviews a local checkout through the local Copilot CLI. The SDK's cloud-session configuration is available but there is no cloud entitlement, repository-association, Mission Control URL, or cloud-session picker in the TUI. |
| [Getting started](https://docs.github.com/en/copilot/how-tos/copilot-sdk/getting-started) | **Implemented** | `Client::start` uses the configured Copilot CLI path, creates/resumes a `Session`, supplies a working directory, a system message, a read-only permission handler, and streaming. |
| [Session persistence](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/session-persistence) | **Partial** | The app persists the MAIN session ID in SQLite, resumes it, reloads SDK history, and applies `continue_pending_work=true` on resume. An unfinished resumed turn receives a synthetic local outbound so continued deltas remain visible. Failed app deliveries remain explicitly recoverable rather than silently resent, and waiting application-owned queue entries are restored from the SQLite outbox. Authenticated evidence for every resumed control path remains incomplete. |
| [Skills](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/skills) | **Implemented** | Global and repository `.rq-tui/skills` directories are passed through `with_skill_directories`; skill invocation and progress events are surfaced as typed activity. |
| [Steering and queueing](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/steering-and-queueing) | **Partial** | New questions remain editable in a durable application-owned FIFO while a turn runs, survive process restart in SQLite, then are delivered with SDK `DeliveryMode::Enqueue`. `/steer` and `:steer` use `DeliveryMode::Immediate`, with normal queue fallback when idle. Correlated typed accepted/failed events settle only their own durable record and cannot rewind a newer active turn; SDK-returned steering message IDs are registered as trusted event-chain roots. Cross-lane FIFO presentation and authenticated restart evidence remain incomplete. |
| [Streaming events](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/streaming-events) | **Partial** | Streaming is enabled; text deltas/snapshots, final messages, task completion, background-task state, usage, common tool progress, skills, subagents, retries, errors, and quiet/liveness metadata are routed through the generic agent interface and rendered. Dispatched and steered requests bind through their SDK message roots; lagged/closed subscriptions fail visible work. Permission/user-input, session-limit, compaction, plan-mode, and several external-tool event shapes still need explicit coverage. |
| [Session limits](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/session-limits) | **Gap** | SDK `SessionLimitsConfig` exists in 1.0.8, but the app does not configure limits or render limit/allowance state. |
| [Remote sessions](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/remote-sessions) | **Intentionally optional/out-of-scope** | A local review session is the default product mode. Remote export/steering is not configured and has no corresponding UI or credential policy. |
| [Plugin directories](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/plugin-directories) | **Implemented** | Global `plugins/` and repository `.rq-tui/plugins` directories are passed on session create and resume. `COPILOT_PLUGIN_DIR_ONLY=true` prevents ambient host plugins from changing the review session. Discovery is filesystem-based rather than a separate picker. |
| [MCP](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/mcp) | **Gap** | The SDK supports MCP server configuration and MCP hooks, and the hook bridge can label MCP calls, but the application does not yet load or configure user MCP servers. |
| [Image input](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/image-input) | **Gap** | The Rust SDK has attachment types and message attachment support, but the TUI has no image/file attachment command, picker, or persistence path. |
| [Hooks](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/hooks) | **Partial** | `SessionHooks` is installed on create and resume and feeds activity into the visible timeline. Most hook outputs are deliberately observational; only error handling currently changes runtime behavior. |
| [Fleet mode](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/fleet-mode) | **Intentionally optional/out-of-scope** | The product has one focused review session and a separate ephemeral `/side` lane, not a multi-agent fleet orchestrator. The SDK fleet API is available but intentionally not enabled. |
| [Custom agents](https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/custom-agents) | **Gap** | Subagent lifecycle events are visible when emitted by the CLI, but the app has no custom-agent configuration or agent picker. |
| [Error-handling hook](https://docs.github.com/en/copilot/how-tos/copilot-sdk/hooks/error-handling) | **Partial** | `ErrorOccurred` events become visible and return a one-retry policy for recoverable errors or abort for non-recoverable errors. There is no application-level retry budget or provider/quota-specific recovery UI. |
| [Hooks overview](https://docs.github.com/en/copilot/how-tos/copilot-sdk/hooks/hooks-overview) | **Partial** | The bridge uses the SDK hook trait instead of inferring all activity from assistant text. Several lifecycle/request event types still lack explicit UI semantics. |
| [Post-tool use](https://docs.github.com/en/copilot/how-tos/copilot-sdk/hooks/post-tool-use) | **Partial** | Successful and failed post-tool hook events become completed/retry activity. Tool result transformation and explicit redaction are not configured. |
| [Pre-tool use](https://docs.github.com/en/copilot/how-tos/copilot-sdk/hooks/pre-tool-use) | **Partial** | Pre-tool and pre-MCP events are visible and the separate permission handler allows reads/searches while denying writes and shell. The hook text must not describe an operation as accepted before the permission result is known. |
| [Session lifecycle hooks](https://docs.github.com/en/copilot/how-tos/copilot-sdk/hooks/session-lifecycle) | **Implemented** | Session start/end lifecycle hooks are recorded in the lane timeline, including the ephemeral SIDE lifecycle. Closing SIDE disconnects and attempts SDK `delete_session`; a timeout/failure is retained visibly after MAIN is restored. |
| [User prompt submitted](https://docs.github.com/en/copilot/how-tos/copilot-sdk/hooks/user-prompt-submitted) | **Implemented** | Prompt-submitted hook activity is recorded, while the app separately persists outbound delivery state before sending and acknowledges it when a response starts. |

## Application-specific audit notes

### Queue, steering, and background behavior

The TUI remains responsive while Copilot is working. Additional questions enter
an explicit FIFO queue and are visible through `:queue`; they do not block the
composer. `/steer <correction>` or `:steer <correction>` sends an immediate
correction to the active SDK loop. If there is no active loop, the correction is
treated as an ordinary queued prompt. In `:queue`, `d` cancels the selected
waiting prompt and `s` stops the active prompt. `Ctrl-C`, `:stop`, `:abort`, and
the agent-status action also request active-turn cancellation.

Named SDK control operations use a 15-second async timeout. The production SDK
worker also runs on a separate OS thread, so a CLI-side synchronous startup
stall cannot freeze terminal input or rendering; the liveness panel continues
to show the current operation and elapsed quiet time.

### Model selection

`:model` opens a capability-driven, three-stage picker: runtime model, that
model's advertised reasoning effort, then its advertised context tier and
capacity. The UI does not invent reasoning levels or long-context options.
Selections are applied through `list_models` plus `set_model` and persisted as
`model`, `model.reasoning_effort`, and `model.context_tier` only after the
runtime acknowledges the change. `Esc` walks back one picker stage.

### MAIN and SIDE

MAIN is the persistent lane. `/side` creates an ephemeral fork, adds an
explicit reference-only MAIN-history boundary to the first side prompt, and
keeps side events and transcript entries lane-filtered. `/main` disconnects the
side session and attempts `delete_session`; MAIN queued work is preserved and
any cleanup failure remains visible after the return.

### Testability and evidence boundary

The Copilot SDK is behind the `AgentRuntime`/`AgentSink` seam. The controlled
agent and `TuiHarness` exercise queueing, steering, streaming, hooks/activity,
model capabilities, liveness, and SIDE state without production credentials.
The deterministic UI gallery is available with:

```text
bazel run //:rq-tui -- ui-snapshot --state all --width 100 --height 28
```

The final regular-test count and current PTY/authenticated-live evidence are
recorded in [`behavioral-audit.md`](behavioral-audit.md) and
[`audit/pty-smoke.md`](audit/pty-smoke.md).

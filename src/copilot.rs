use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use github_copilot_sdk::handler::{PermissionHandler, PermissionResult};
use github_copilot_sdk::hooks::{ErrorOccurredOutput, HookEvent, HookOutput, SessionHooks};
use github_copilot_sdk::rpc::{HistoryCompactRequest, SessionsForkRequest};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::subscription::RecvErrorKind;
use github_copilot_sdk::{
    CliProgram, Client, ClientOptions, ContextTier, DeliveryMode, MessageOptions,
    ResumeSessionConfig, SessionConfig, SessionEvent, SessionId, SetModelOptions,
    SystemMessageConfig,
};
use github_copilot_sdk::{PermissionRequestData, PermissionRequestKind, RequestId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

use crate::config::AppPaths;
use crate::domain::{EphemeralSessionRecord, SessionRecord};
use crate::prune::prune_work_item;
use crate::storage::{now, PruneOperation, Storage};

const SIDE_BOUNDARY: &str = "Side conversation boundary.\n\
Everything before this boundary is inherited MAIN history and is reference context only, not the \
current task. Answer only the side question below. Do not continue plans or instructions from MAIN. \
This SIDE conversation is ephemeral and read-only; do not modify files or workspace state.";
const SDK_CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const SIDE_LEASE_TTL: Duration = Duration::from_secs(30);
const SIDE_LEASE_HEARTBEAT: Duration = Duration::from_secs(5);
const SIDE_CLEANUP_RETRY: Duration = Duration::from_secs(30);

fn apply_side_boundary(outbound: &mut Outbound) {
    outbound.text = format!("{SIDE_BOUNDARY}\n\nSide question:\n{}", outbound.text);
}

#[derive(Clone, Debug)]
pub(crate) struct BridgeConfig {
    pub(crate) work_item_id: String,
    pub(crate) session_root: PathBuf,
    pub(crate) database_path: PathBuf,
    pub(crate) app_paths: AppPaths,
    pub(crate) existing_session_id: Option<String>,
    pub(crate) model: String,
    /// Optional runtime-approved thinking level selected after the model.
    pub(crate) reasoning_effort: Option<String>,
    /// Optional runtime-approved context tier selected after thinking level.
    pub(crate) context_tier: Option<String>,
    pub(crate) skill_directories: Vec<PathBuf>,
    pub(crate) plugin_directories: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OutboundKind {
    Ask {
        annotation_id: String,
        user_message_id: String,
        assistant_message_id: String,
        assistant_seq: i64,
    },
    CommentBatch {
        annotation_ids: Vec<String>,
    },
    Chat,
    ContextDraft,
    Context {
        work_item_id: String,
    },
    Correction,
}

/// The conversation that owns an outbound turn or SDK event. Main is the only
/// persisted lane; side sessions intentionally disappear on bridge restart.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AgentLane {
    Main,
    Side { id: String },
}

impl AgentLane {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Main => "MAIN".into(),
            Self::Side { id } => format!("SIDE {id}"),
        }
    }
}

/// Machine-readable activity phases for a progress UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivityKind {
    Intent,
    Reasoning,
    ToolStart,
    ToolProgress,
    ToolComplete,
    Retry,
    Failure,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentActivity {
    pub(crate) kind: ActivityKind,
    pub(crate) label: String,
    pub(crate) tool: Option<String>,
    pub(crate) detail: Option<String>,
}

impl AgentActivity {
    fn other(label: impl Into<String>) -> Self {
        Self {
            kind: ActivityKind::Other,
            label: label.into(),
            tool: None,
            detail: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Outbound {
    pub(crate) id: String,
    pub(crate) kind: OutboundKind,
    pub(crate) text: String,
}

impl Outbound {
    pub(crate) fn new(kind: OutboundKind, text: String) -> Self {
        Self {
            id: next_outbound_id(),
            kind,
            text,
        }
    }
}

thread_local! {
    static DETERMINISTIC_OUTBOUND_COUNTER: Cell<Option<u64>> = const { Cell::new(None) };
}

pub(crate) struct DeterministicOutboundIds {
    previous: Option<u64>,
}

impl Drop for DeterministicOutboundIds {
    fn drop(&mut self) {
        DETERMINISTIC_OUTBOUND_COUNTER.with(|counter| counter.set(self.previous));
    }
}

pub(crate) fn deterministic_outbound_ids() -> DeterministicOutboundIds {
    let previous = DETERMINISTIC_OUTBOUND_COUNTER.with(|counter| counter.replace(Some(0)));
    DeterministicOutboundIds { previous }
}

fn next_outbound_id() -> String {
    DETERMINISTIC_OUTBOUND_COUNTER
        .with(|counter| {
            counter.get().map(|current| {
                let next = current.saturating_add(1);
                counter.set(Some(next));
                format!("{next:08x}-0000-4000-8000-000000000000")
            })
        })
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn model_option(model: &github_copilot_sdk::Model) -> ModelOption {
    let max_context_tokens = model
        .capabilities
        .limits
        .as_ref()
        .and_then(|limits| limits.max_context_window_tokens);
    let max_output_tokens = model
        .capabilities
        .limits
        .as_ref()
        .and_then(|limits| limits.max_output_tokens);
    let mut context_tiers = vec![ContextTierOption {
        id: "default".into(),
        max_context_tokens,
    }];
    if let Some(long_context) = model
        .billing
        .as_ref()
        .and_then(|billing| billing.token_prices.as_ref())
        .and_then(|prices| prices.long_context.as_ref())
    {
        context_tiers.push(ContextTierOption {
            id: "long_context".into(),
            max_context_tokens: match (long_context.max_prompt_tokens, max_output_tokens) {
                (Some(prompt), Some(output)) => Some(prompt + output),
                _ => long_context.max_prompt_tokens,
            },
        });
    }
    ModelOption {
        id: model.id.clone(),
        name: model.name.clone(),
        supported_reasoning_efforts: model
            .supported_reasoning_efforts
            .clone()
            .unwrap_or_default(),
        default_reasoning_effort: model.default_reasoning_effort.clone(),
        max_context_tokens,
        context_tiers,
    }
}

fn context_tier_from_wire(value: &str) -> Result<ContextTier> {
    match value {
        "default" => Ok(ContextTier::Default),
        "long_context" => Ok(ContextTier::LongContext),
        _ => anyhow::bail!("unsupported context tier {value:?}; refresh the model list"),
    }
}

fn set_model_options(selection: &ModelSelection) -> Result<SetModelOptions> {
    let mut options = SetModelOptions::default();
    if let Some(effort) = &selection.reasoning_effort {
        options = options.with_reasoning_effort(effort.clone());
    }
    if let Some(tier) = &selection.context_tier {
        options = options.with_context_tier(context_tier_from_wire(tier)?);
    }
    Ok(options)
}

fn enqueue_message(outbound: &Outbound) -> MessageOptions {
    MessageOptions::new(outbound.text.clone()).with_mode(DeliveryMode::Enqueue)
}

fn controlled_models() -> Vec<ModelOption> {
    vec![
        ModelOption {
            id: "controlled-fast".into(),
            name: "Controlled Fast".into(),
            supported_reasoning_efforts: vec!["low".into(), "medium".into()],
            default_reasoning_effort: Some("medium".into()),
            max_context_tokens: Some(32_768),
            context_tiers: vec![ContextTierOption {
                id: "default".into(),
                max_context_tokens: Some(32_768),
            }],
        },
        ModelOption {
            id: "controlled-deep".into(),
            name: "Controlled Deep".into(),
            supported_reasoning_efforts: vec!["medium".into(), "high".into()],
            default_reasoning_effort: Some("high".into()),
            max_context_tokens: Some(128_000),
            context_tiers: vec![
                ContextTierOption {
                    id: "default".into(),
                    max_context_tokens: Some(128_000),
                },
                ContextTierOption {
                    id: "long_context".into(),
                    max_context_tokens: Some(256_000),
                },
            ],
        },
    ]
}

/// Context-tier data returned by the runtime for a selectable model.
///
/// The list is intentionally capability-derived: callers should present only
/// these values and must not invent a long-context option for a model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextTierOption {
    /// SDK wire value: `"default"` or `"long_context"`.
    pub(crate) id: String,
    /// Total context-window capacity when the runtime advertises it.
    pub(crate) max_context_tokens: Option<i64>,
}

/// Runtime-derived model metadata for a progressive picker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelOption {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) supported_reasoning_efforts: Vec<String>,
    pub(crate) default_reasoning_effort: Option<String>,
    /// Default-tier total context window, if the SDK reports one.
    pub(crate) max_context_tokens: Option<i64>,
    /// Selectable context tiers, with their runtime-advertised capacities.
    pub(crate) context_tiers: Vec<ContextTierOption>,
}

/// A complete, staged model choice. Each optional field means "use the
/// runtime's default" rather than a guessed value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelSelection {
    pub(crate) model_id: String,
    pub(crate) reasoning_effort: Option<String>,
    /// SDK wire value: `"default"` or `"long_context"`.
    pub(crate) context_tier: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum AgentCommand {
    Send(Outbound),
    /// Redirect the active agent loop without waiting behind queued prompts.
    Steer(Outbound),
    /// Remove a prompt that has not started yet.
    CancelQueued(String),
    /// Replace a waiting prompt in place. If it already started, reject the
    /// replacement instead of running both requests.
    ReplaceQueued {
        outbound_id: String,
        replacement: Outbound,
    },
    /// Fork the persisted main session and enter an ephemeral side lane.
    StartSide {
        outbound: Option<Outbound>,
    },
    /// Send a follow-up to the currently active side lane.
    SendSide(Outbound),
    /// Cancel a pending SIDE fork without aborting unrelated MAIN work.
    CancelSide,
    /// Drop the current ephemeral side session and return to the main session.
    ExitSide,
    Abort,
    Fork,
    Compact(Option<String>),
    /// Fetch runtime-advertised models and their picker capabilities.
    ListModels,
    /// Apply the staged model -> reasoning effort -> context tier choice.
    SelectModel(ModelSelection),
    /// Delete every persisted Copilot session associated with reviewed Work
    /// Items before their local history is pruned.
    PruneSessions {
        request_id: String,
        work_item_ids: Vec<String>,
        export_first: bool,
        paths: AppPaths,
    },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PruneSessionOutcome {
    pub(crate) work_item_id: String,
    pub(crate) remote_deleted: bool,
    pub(crate) local_deleted: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryEntry {
    pub(crate) role: String,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    SessionReady {
        session_id: String,
        resumed: bool,
        resume_warning: Option<String>,
    },
    HistoryLoaded(Vec<HistoryEntry>),
    Queued {
        outbound_id: String,
        position: usize,
    },
    QueueCancelled {
        outbound_id: String,
    },
    QueueReplaced {
        outbound_id: String,
        replacement_id: String,
        position: usize,
    },
    QueueReplaceRejected {
        outbound_id: String,
        replacement_id: String,
        original_active: bool,
        reason: String,
    },
    SteeringAccepted {
        steering_id: String,
        active_outbound_id: String,
    },
    SteeringFailed {
        steering_id: String,
        active_outbound_id: String,
        message: String,
    },
    ResponseStarted {
        outbound_id: String,
        outbound: OutboundKind,
        first_delta: String,
    },
    ResponseDelta {
        outbound_id: String,
        delta: String,
    },
    ResponseSnapshot {
        outbound_id: String,
        text: String,
    },
    ResponseComplete {
        outbound_id: String,
        aborted: bool,
    },
    /// A stop request reached the runtime after its active turn had already
    /// settled. This is distinct from generic activity because the UI must
    /// leave STOPPING even when no turn-scoped idle event follows.
    StopSettledAlreadyIdle,
    TurnFailed {
        outbound_id: String,
        outbound: OutboundKind,
        message: String,
        response_started: bool,
    },
    Activity {
        outbound_id: Option<String>,
        label: String,
    },
    Usage {
        model: String,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        cache_read_tokens: Option<i64>,
    },
    Forked {
        parent_id: String,
        session_id: String,
    },
    ModelsListed(Vec<ModelOption>),
    /// Detailed selection acknowledgement for staged pickers.
    ModelSelectionChanged(ModelSelection),
    ModelSelectionFailed {
        selection: ModelSelection,
        message: String,
    },
    /// Legacy one-stage acknowledgement retained for existing callers.
    ModelChanged(String),
    Compacted,
    OrphanSideCleanup {
        session_id: String,
        cleanup_warning: Option<String>,
    },
    PruneSessionsStarted {
        request_id: String,
        work_items: usize,
    },
    PruneSessionProgress {
        request_id: String,
        work_item_id: String,
        label: String,
        completed: usize,
        total: usize,
    },
    PruneRecoveryStarted {
        operation_id: String,
        work_item_id: String,
    },
    PruneSessionsComplete {
        request_id: String,
        outcomes: Vec<PruneSessionOutcome>,
    },
    PruneRecovery {
        operation_id: String,
        outcome: PruneSessionOutcome,
    },
    Error(String),
    Stopped,
}

/// The typed event stream used by lane-aware UI code. The legacy `try_recv`
/// projection remains available while callers migrate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaneEvent {
    Agent(AgentEvent),
    SideStarted {
        parent_id: String,
        side_id: String,
    },
    SideExited {
        parent_id: String,
        side_id: String,
        cleanup_warning: Option<String>,
    },
    SideCancelled,
    SideFailed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentEventEnvelope {
    pub(crate) lane: AgentLane,
    pub(crate) event: LaneEvent,
    pub(crate) activity: Option<AgentActivity>,
}

impl AgentEventEnvelope {
    fn agent(lane: AgentLane, event: AgentEvent) -> Self {
        Self {
            lane,
            event: LaneEvent::Agent(event),
            activity: None,
        }
    }

    fn activity(lane: AgentLane, outbound_id: Option<String>, activity: AgentActivity) -> Self {
        Self {
            lane,
            event: LaneEvent::Agent(AgentEvent::Activity {
                outbound_id,
                label: activity.label.clone(),
            }),
            activity: Some(activity),
        }
    }

    fn legacy(self) -> AgentEvent {
        match self.event {
            LaneEvent::Agent(event) => event,
            LaneEvent::SideStarted { side_id, .. } => AgentEvent::Activity {
                outbound_id: None,
                label: format!("SIDE {side_id} started (ephemeral; main history is unchanged)"),
            },
            LaneEvent::SideExited { side_id, .. } => AgentEvent::Activity {
                outbound_id: None,
                label: format!("SIDE {side_id} closed; back on MAIN"),
            },
            LaneEvent::SideCancelled => AgentEvent::Activity {
                outbound_id: None,
                label: "SIDE creation cancelled; MAIN is unchanged".into(),
            },
            LaneEvent::SideFailed { message } => AgentEvent::Error(message),
        }
    }
}

pub(crate) struct CopilotBridge {
    commands: UnboundedSender<AgentCommand>,
    events: Receiver<AgentEventEnvelope>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) trait AgentSink {
    fn send(&self, command: AgentCommand) -> Result<()>;
}

pub(crate) trait AgentRuntime: AgentSink {
    fn try_recv(&self) -> Option<AgentEvent>;

    fn try_recv_laned(&self) -> Option<AgentEventEnvelope> {
        self.try_recv()
            .map(|event| AgentEventEnvelope::agent(AgentLane::Main, event))
    }
}

impl CopilotBridge {
    pub(crate) fn start(config: BridgeConfig) -> Self {
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        // The worker can fail outside its async loop (for example while its
        // runtime is being created). Keep its most recently visible lane so
        // that a fatal Error/Stopped event is not silently filtered out while
        // the UI is showing an ephemeral SIDE conversation.
        let current_lane = Arc::new(std::sync::Mutex::new(AgentLane::Main));
        let worker_lane = current_lane.clone();
        let thread = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = event_tx.send(AgentEventEnvelope::agent(
                        tracked_lane(&current_lane),
                        AgentEvent::Error(error.to_string()),
                    ));
                    return;
                }
            };
            if let Err(error) =
                runtime.block_on(worker(config, command_rx, event_tx.clone(), worker_lane))
            {
                let _ = event_tx.send(AgentEventEnvelope::agent(
                    tracked_lane(&current_lane),
                    AgentEvent::Error(format!("{error:#}")),
                ));
            }
            let _ = event_tx.send(AgentEventEnvelope::agent(
                tracked_lane(&current_lane),
                AgentEvent::Stopped,
            ));
        });
        Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    pub(crate) fn send(&self, command: AgentCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("Copilot worker has stopped"))
    }

    pub(crate) fn try_recv(&self) -> Option<AgentEvent> {
        self.try_recv_laned().map(AgentEventEnvelope::legacy)
    }

    pub(crate) fn try_recv_laned(&self) -> Option<AgentEventEnvelope> {
        self.events.try_recv().ok()
    }
}

impl AgentSink for CopilotBridge {
    fn send(&self, command: AgentCommand) -> Result<()> {
        CopilotBridge::send(self, command)
    }
}

impl AgentRuntime for CopilotBridge {
    fn try_recv(&self) -> Option<AgentEvent> {
        CopilotBridge::try_recv(self)
    }

    fn try_recv_laned(&self) -> Option<AgentEventEnvelope> {
        CopilotBridge::try_recv_laned(self)
    }
}

pub(crate) fn start_agent(config: BridgeConfig) -> Box<dyn AgentRuntime> {
    if env::var_os("RQ_TUI_CONTROLLED_AGENT").as_deref() == Some(std::ffi::OsStr::new("1")) {
        Box::new(ControlledAgent::with_config(config))
    } else {
        Box::new(CopilotBridge::start(config))
    }
}

struct ControlledAgent {
    work_item_id: String,
    state: Arc<std::sync::Mutex<ControlledState>>,
    _process_lock: Option<WorkItemProcessLock>,
}

struct ControlledState {
    events: VecDeque<(Instant, AgentEventEnvelope)>,
    active_side: Option<String>,
    next_side: u64,
    busy_until: Instant,
    outbound_lanes: HashMap<String, AgentLane>,
    active_outbound_id: Option<String>,
}

impl ControlledAgent {
    const STARTUP_FLOOD_EVENTS: usize = 1_024;

    #[cfg(test)]
    fn new(work_item_id: String) -> Self {
        Self::build(work_item_id, true)
    }

    fn build(work_item_id: String, session_ready: bool) -> Self {
        let mut events = VecDeque::new();
        if session_ready {
            events.push_back((
                Instant::now(),
                AgentEventEnvelope::agent(
                    AgentLane::Main,
                    AgentEvent::SessionReady {
                        session_id: format!("controlled-{work_item_id}"),
                        resumed: false,
                        resume_warning: None,
                    },
                ),
            ));
        }
        Self {
            work_item_id,
            state: Arc::new(std::sync::Mutex::new(ControlledState {
                events,
                active_side: None,
                next_side: 1,
                busy_until: Instant::now(),
                outbound_lanes: HashMap::new(),
                active_outbound_id: None,
            })),
            _process_lock: None,
        }
    }

    fn with_config(config: BridgeConfig) -> Self {
        let mut agent = Self::build(config.work_item_id.clone(), false);
        match WorkItemProcessLock::try_acquire(&config.app_paths, &config.work_item_id) {
            Ok(Some(lock)) => agent._process_lock = Some(lock),
            Ok(None) => {
                agent.schedule_agent(
                    AgentLane::Main,
                    Duration::ZERO,
                    AgentEvent::Error(
                        "another controlled rq-tui process is already using this Work Item".into(),
                    ),
                );
                return agent;
            }
            Err(error) => {
                agent.schedule_agent(
                    AgentLane::Main,
                    Duration::ZERO,
                    AgentEvent::Error(format!(
                        "controlled startup could not acquire its Work Item lock: {error}"
                    )),
                );
                return agent;
            }
        }
        let state = Arc::clone(&agent.state);
        let flood_startup = env::var_os("RQ_TUI_CONTROLLED_STARTUP_FLOOD").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        std::thread::spawn(move || {
            let storage = match Storage::open(&config.database_path) {
                Ok(storage) => storage,
                Err(error) => {
                    Self::schedule_shared(
                        &state,
                        Duration::ZERO,
                        AgentEventEnvelope::agent(
                            AgentLane::Main,
                            AgentEvent::Error(format!(
                                "controlled startup could not open recovery storage: {error}"
                            )),
                        ),
                    );
                    return;
                }
            };
            let operations = match storage.pending_prune_operations() {
                Ok(operations) => operations,
                Err(error) => {
                    Self::schedule_shared(
                        &state,
                        Duration::ZERO,
                        AgentEventEnvelope::agent(
                            AgentLane::Main,
                            AgentEvent::Error(format!(
                                "controlled startup could not inspect prune recovery: {error}"
                            )),
                        ),
                    );
                    return;
                }
            };
            if operations
                .iter()
                .any(|operation| operation.work_item_id == config.work_item_id)
            {
                Self::schedule_shared(
                    &state,
                    Duration::ZERO,
                    AgentEventEnvelope::agent(
                        AgentLane::Main,
                        AgentEvent::Error(
                            "this Work Item has an interrupted prune operation and cannot open"
                                .into(),
                        ),
                    ),
                );
                return;
            }
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    Self::schedule_shared(
                        &state,
                        Duration::ZERO,
                        AgentEventEnvelope::agent(
                            AgentLane::Main,
                            AgentEvent::Error(format!(
                                "controlled startup could not create its recovery runtime: {error}"
                            )),
                        ),
                    );
                    return;
                }
            };
            for operation in operations {
                let operation_id = operation.operation_id.clone();
                let work_item_id = operation.work_item_id.clone();
                Self::schedule_shared(
                    &state,
                    Duration::ZERO,
                    AgentEventEnvelope::agent(
                        AgentLane::Main,
                        AgentEvent::PruneRecoveryStarted {
                            operation_id: operation_id.clone(),
                            work_item_id: work_item_id.clone(),
                        },
                    ),
                );
                let progress_state = Arc::clone(&state);
                let progress_operation = operation_id.clone();
                let progress_item = work_item_id.clone();
                let outcome = runtime.block_on(execute_prune_operation(
                    &ControlledCleanupBackend,
                    PruneExecution {
                        ledger: &storage,
                        database_path: &config.database_path,
                        owner_id: "controlled-recovery-owner",
                        requested_operation_id: &operation_id,
                        work_item_id: &work_item_id,
                        export_first: operation.export_first,
                        paths: &config.app_paths,
                    },
                    move |label, completed, total| {
                        Self::schedule_shared(
                            &progress_state,
                            Duration::ZERO,
                            AgentEventEnvelope::agent(
                                AgentLane::Main,
                                AgentEvent::PruneSessionProgress {
                                    request_id: progress_operation.clone(),
                                    work_item_id: progress_item.clone(),
                                    label,
                                    completed,
                                    total,
                                },
                            ),
                        );
                    },
                ));
                Self::schedule_shared(
                    &state,
                    Duration::ZERO,
                    AgentEventEnvelope::agent(
                        AgentLane::Main,
                        AgentEvent::PruneRecovery {
                            operation_id,
                            outcome,
                        },
                    ),
                );
            }
            if flood_startup {
                for _ in 0..Self::STARTUP_FLOOD_EVENTS {
                    Self::schedule_shared(
                        &state,
                        Duration::ZERO,
                        AgentEventEnvelope::agent(
                            AgentLane::Main,
                            AgentEvent::Activity {
                                outbound_id: None,
                                label: "Controlled startup event flood".into(),
                            },
                        ),
                    );
                }
            }
            Self::schedule_shared(
                &state,
                Duration::ZERO,
                AgentEventEnvelope::agent(
                    AgentLane::Main,
                    AgentEvent::SessionReady {
                        session_id: format!("controlled-{}", config.work_item_id),
                        resumed: false,
                        resume_warning: None,
                    },
                ),
            );
        });
        agent
    }

    fn schedule(&self, delay: Duration, event: AgentEventEnvelope) {
        Self::schedule_shared(&self.state, delay, event);
    }

    fn schedule_shared(
        state: &Arc<std::sync::Mutex<ControlledState>>,
        delay: Duration,
        event: AgentEventEnvelope,
    ) {
        let available_at = Instant::now() + delay;
        let mut state = state.lock().expect("controlled agent lock");
        let position = state
            .events
            .iter()
            .position(|(scheduled, _)| *scheduled > available_at)
            .unwrap_or(state.events.len());
        state.events.insert(position, (available_at, event));
    }

    fn schedule_agent(&self, lane: AgentLane, delay: Duration, event: AgentEvent) {
        self.schedule(delay, AgentEventEnvelope::agent(lane, event));
    }

    fn schedule_activity(
        &self,
        lane: AgentLane,
        delay: Duration,
        outbound_id: Option<String>,
        activity: AgentActivity,
    ) {
        self.schedule(
            delay,
            AgentEventEnvelope::activity(lane, outbound_id, activity),
        );
    }

    fn schedule_turn(&self, lane: AgentLane, outbound: Outbound, delay: Duration) {
        let context_draft = matches!(&outbound.kind, OutboundKind::ContextDraft);
        let audit_events = env::var_os("RQ_TUI_CONTROLLED_AUDIT_EVENTS").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        let first = if context_draft {
            "Title: Controlled review\nWhat: Deterministic context\n".to_owned()
        } else {
            "Controlled ".to_owned()
        };
        let second = if context_draft {
            "Why: Repeatable tests\nHow: Fake streaming\nConsiderations: None\nOther approaches: Live agent".to_owned()
        } else if audit_events {
            (0..350)
                .map(|index| format!(" audit-token-{index}"))
                .collect()
        } else {
            "streamed response.".to_owned()
        };
        let delta_delay = if audit_events {
            Duration::from_secs(4)
        } else {
            Duration::from_secs(12)
        };
        let complete_delay = if audit_events {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(16)
        };
        let id = outbound.id.clone();
        self.schedule_agent(
            lane.clone(),
            delay,
            AgentEvent::Queued {
                outbound_id: id.clone(),
                position: 0,
            },
        );
        self.schedule_activity(
            lane.clone(),
            delay + Duration::from_millis(100),
            Some(id.clone()),
            AgentActivity {
                kind: ActivityKind::Intent,
                label: "Planning response".into(),
                tool: None,
                detail: None,
            },
        );
        self.schedule_activity(
            lane.clone(),
            delay + Duration::from_millis(400),
            Some(id.clone()),
            AgentActivity {
                kind: ActivityKind::ToolStart,
                label: "Using read_file".into(),
                tool: Some("read_file".into()),
                detail: Some("Inspecting selected context".into()),
            },
        );
        if audit_events {
            self.schedule_activity(
                lane.clone(),
                delay + Duration::from_millis(500),
                Some(id.clone()),
                AgentActivity::other("Running review skill: exhaustive audit"),
            );
            self.schedule_activity(
                lane.clone(),
                delay + Duration::from_millis(600),
                Some(id.clone()),
                AgentActivity::other("Subagent inspecting terminal edge cases"),
            );
            self.schedule_activity(
                lane.clone(),
                delay + Duration::from_millis(700),
                Some(id.clone()),
                AgentActivity {
                    kind: ActivityKind::Retry,
                    label: "Retrying transient controlled operation".into(),
                    tool: None,
                    detail: Some("attempt 2/3".into()),
                },
            );
        }
        self.schedule_activity(
            lane.clone(),
            delay + Duration::from_millis(800),
            Some(id.clone()),
            AgentActivity {
                kind: ActivityKind::ToolComplete,
                label: "Finished read_file".into(),
                tool: Some("read_file".into()),
                detail: None,
            },
        );
        self.schedule_agent(
            lane.clone(),
            delay + Duration::from_secs(2),
            AgentEvent::ResponseStarted {
                outbound_id: id.clone(),
                outbound: outbound.kind,
                first_delta: first,
            },
        );
        self.schedule_agent(
            lane.clone(),
            delay + delta_delay,
            AgentEvent::ResponseDelta {
                outbound_id: id.clone(),
                delta: second,
            },
        );
        self.schedule_agent(
            lane.clone(),
            delay + complete_delay,
            AgentEvent::ResponseComplete {
                outbound_id: id.clone(),
                aborted: false,
            },
        );
        let mut state = self.state.lock().expect("controlled agent lock");
        state.outbound_lanes.insert(id.clone(), lane);
        state.busy_until = state
            .busy_until
            .max(Instant::now() + delay + complete_delay);
    }
}

impl AgentSink for ControlledAgent {
    fn send(&self, command: AgentCommand) -> Result<()> {
        match command {
            AgentCommand::Send(outbound) => {
                let delay = self
                    .state
                    .lock()
                    .expect("controlled agent lock")
                    .busy_until
                    .saturating_duration_since(Instant::now());
                self.schedule_turn(AgentLane::Main, outbound, delay)
            }
            AgentCommand::Steer(outbound) => {
                let steering_id = outbound.id.clone();
                let (lane, outbound_id) = {
                    let state = self.state.lock().expect("controlled agent lock");
                    let outbound_id = state.active_outbound_id.clone().or_else(|| {
                        state.events.iter().find_map(|(_, envelope)| {
                            matches!(envelope.event, LaneEvent::Agent(AgentEvent::Queued { .. }))
                                .then(|| envelope_outbound_id(envelope))
                                .flatten()
                        })
                    });
                    let lane = outbound_id
                        .as_ref()
                        .and_then(|id| state.outbound_lanes.get(id))
                        .cloned()
                        .or_else(|| state.active_side.clone().map(|id| AgentLane::Side { id }))
                        .unwrap_or(AgentLane::Main);
                    (lane, outbound_id)
                };
                if let Some(active_outbound_id) = outbound_id {
                    self.schedule_agent(
                        lane,
                        Duration::ZERO,
                        AgentEvent::SteeringAccepted {
                            steering_id,
                            active_outbound_id,
                        },
                    );
                }
            }
            AgentCommand::CancelQueued(outbound_id) => {
                let (lane, cancelled) = {
                    let mut state = self.state.lock().expect("controlled agent lock");
                    let queued = state.events.iter().any(|(_, envelope)| {
                        matches!(
                            &envelope.event,
                            LaneEvent::Agent(AgentEvent::Queued {
                                outbound_id: queued_id,
                                ..
                            }) if queued_id == &outbound_id
                        )
                    });
                    let lane = state.outbound_lanes.get(&outbound_id).cloned();
                    if queued {
                        state.events.retain(|(_, envelope)| {
                            envelope_outbound_id(envelope).as_deref() != Some(outbound_id.as_str())
                        });
                        state.outbound_lanes.remove(&outbound_id);
                    }
                    (lane, queued)
                };
                if cancelled {
                    self.schedule_agent(
                        lane.unwrap_or(AgentLane::Main),
                        Duration::ZERO,
                        AgentEvent::QueueCancelled { outbound_id },
                    );
                } else {
                    self.schedule_activity(
                        lane.unwrap_or(AgentLane::Main),
                        Duration::ZERO,
                        Some(outbound_id),
                        AgentActivity::other("Prompt was already active or no longer queued"),
                    );
                }
            }
            AgentCommand::ReplaceQueued {
                outbound_id,
                replacement,
            } => {
                let replacement_id = replacement.id.clone();
                let (scheduled, owner, original_active) = {
                    let mut state = self.state.lock().expect("controlled agent lock");
                    let queued = state.events.iter().find_map(|(available_at, envelope)| {
                        matches!(
                            &envelope.event,
                            LaneEvent::Agent(AgentEvent::Queued {
                                outbound_id: queued_id,
                                ..
                            }) if queued_id == &outbound_id
                        )
                        .then(|| (*available_at, envelope.lane.clone()))
                    });
                    let owner = state.outbound_lanes.get(&outbound_id).cloned();
                    let original_active = queued.is_none()
                        && (state.active_outbound_id.as_deref() == Some(outbound_id.as_str())
                            || state.events.iter().any(|(_, envelope)| {
                                envelope_outbound_id(envelope).as_deref()
                                    == Some(outbound_id.as_str())
                            }));
                    if queued.is_some() {
                        state.events.retain(|(_, envelope)| {
                            envelope_outbound_id(envelope).as_deref() != Some(outbound_id.as_str())
                        });
                        state.outbound_lanes.remove(&outbound_id);
                    }
                    (queued, owner, original_active)
                };
                if let Some((available_at, lane)) = scheduled {
                    let delay = available_at.saturating_duration_since(Instant::now());
                    self.schedule_agent(
                        lane.clone(),
                        Duration::ZERO,
                        AgentEvent::QueueReplaced {
                            outbound_id,
                            replacement_id,
                            position: 0,
                        },
                    );
                    self.schedule_turn(lane, replacement, delay);
                } else {
                    let lane = owner.unwrap_or_else(|| {
                        self.state
                            .lock()
                            .expect("controlled agent lock")
                            .active_side
                            .clone()
                            .map(|id| AgentLane::Side { id })
                            .unwrap_or(AgentLane::Main)
                    });
                    self.schedule_agent(
                        lane,
                        Duration::ZERO,
                        AgentEvent::QueueReplaceRejected {
                            outbound_id,
                            replacement_id,
                            original_active,
                            reason: if original_active {
                                "prompt already started".into()
                            } else {
                                "prompt already left the queue".into()
                            },
                        },
                    );
                }
            }
            AgentCommand::StartSide { outbound } => {
                let (side_id, parent_id, delay) = {
                    let mut state = self.state.lock().expect("controlled agent lock");
                    let side_id = format!("{}", state.next_side);
                    state.next_side += 1;
                    state.active_side = Some(side_id.clone());
                    (
                        side_id,
                        "controlled-main".to_owned(),
                        state.busy_until.saturating_duration_since(Instant::now()),
                    )
                };
                let lane = AgentLane::Side {
                    id: side_id.clone(),
                };
                self.schedule(
                    delay,
                    AgentEventEnvelope {
                        lane: lane.clone(),
                        event: LaneEvent::SideStarted { parent_id, side_id },
                        activity: None,
                    },
                );
                self.schedule_activity(
                    lane.clone(),
                    delay,
                    None,
                    AgentActivity::other("Side session ready; MAIN history is unchanged"),
                );
                if let Some(outbound) = outbound {
                    self.schedule_turn(lane, outbound, delay);
                }
            }
            AgentCommand::SendSide(outbound) => {
                let side = self
                    .state
                    .lock()
                    .expect("controlled agent lock")
                    .active_side
                    .clone();
                if let Some(id) = side {
                    let delay = self
                        .state
                        .lock()
                        .expect("controlled agent lock")
                        .busy_until
                        .saturating_duration_since(Instant::now());
                    self.schedule_turn(AgentLane::Side { id }, outbound, delay);
                } else {
                    self.schedule_activity(
                        AgentLane::Main,
                        Duration::ZERO,
                        None,
                        AgentActivity::other("No active SIDE session; use /side first"),
                    );
                }
            }
            AgentCommand::CancelSide => {
                let mut state = self.state.lock().expect("controlled agent lock");
                let side_id = state.active_side.take();
                if let Some(side_id) = side_id {
                    state.events.retain(|(_, envelope)| {
                        !matches!(
                            &envelope.lane,
                            AgentLane::Side { id } if id == &side_id
                        )
                    });
                }
                state.busy_until = Instant::now();
                drop(state);
                self.schedule(
                    Duration::ZERO,
                    AgentEventEnvelope {
                        lane: AgentLane::Main,
                        event: LaneEvent::SideCancelled,
                        activity: None,
                    },
                );
            }
            AgentCommand::ExitSide => {
                let side = {
                    let mut state = self.state.lock().expect("controlled agent lock");
                    let side = state.active_side.take();
                    if let Some(side_id) = &side {
                        state.events.retain(|(_, envelope)| {
                            !matches!(&envelope.lane, AgentLane::Side { id } if id == side_id)
                        });
                    }
                    state.busy_until = Instant::now();
                    side
                };
                if let Some(side_id) = side {
                    self.schedule(
                        Duration::ZERO,
                        AgentEventEnvelope {
                            lane: AgentLane::Side {
                                id: side_id.clone(),
                            },
                            event: LaneEvent::SideExited {
                                parent_id: "controlled-main".into(),
                                side_id,
                                cleanup_warning: None,
                            },
                            activity: None,
                        },
                    );
                }
            }
            AgentCommand::Abort => {
                let (lane, aborted_id) = {
                    let mut state = self.state.lock().expect("controlled agent lock");
                    let target = state
                        .events
                        .iter()
                        .find_map(|(_, envelope)| envelope_outbound_id(envelope));
                    let lane = target
                        .as_ref()
                        .and_then(|target| {
                            state
                                .events
                                .iter()
                                .find(|(_, envelope)| {
                                    envelope_outbound_id(envelope).as_ref() == Some(target)
                                })
                                .map(|(_, envelope)| envelope.lane.clone())
                        })
                        .or_else(|| state.active_side.clone().map(|id| AgentLane::Side { id }))
                        .unwrap_or(AgentLane::Main);
                    if let Some(target) = &target {
                        state.events.retain(|(_, envelope)| {
                            envelope_outbound_id(envelope).as_ref() != Some(target)
                        });
                        let now = Instant::now();
                        if let Some(next_turn_at) = state
                            .events
                            .iter()
                            .filter(|(_, envelope)| envelope_outbound_id(envelope).is_some())
                            .map(|(scheduled, _)| *scheduled)
                            .min()
                        {
                            let advance = next_turn_at.saturating_duration_since(now);
                            if !advance.is_zero() {
                                for (scheduled, envelope) in &mut state.events {
                                    if envelope_outbound_id(envelope).is_some() {
                                        *scheduled =
                                            scheduled.checked_sub(advance).unwrap_or(now).max(now);
                                    }
                                }
                                state.busy_until = state
                                    .busy_until
                                    .checked_sub(advance)
                                    .unwrap_or(now)
                                    .max(now);
                            }
                        } else {
                            state.busy_until = now;
                        }
                    }
                    (lane, target)
                };
                self.schedule_activity(
                    lane.clone(),
                    Duration::ZERO,
                    aborted_id.clone(),
                    AgentActivity::other("Cancellation acknowledged by controlled SDK"),
                );
                if let Some(outbound_id) = aborted_id {
                    self.schedule_agent(
                        lane,
                        Duration::from_millis(1),
                        AgentEvent::ResponseComplete {
                            outbound_id,
                            aborted: true,
                        },
                    );
                } else {
                    self.schedule_agent(
                        lane,
                        Duration::from_millis(1),
                        AgentEvent::StopSettledAlreadyIdle,
                    );
                }
            }
            AgentCommand::Fork => self.schedule_activity(
                AgentLane::Main,
                Duration::ZERO,
                None,
                AgentActivity::other("Controlled session forked"),
            ),
            AgentCommand::Compact(_) => {
                self.schedule_agent(AgentLane::Main, Duration::ZERO, AgentEvent::Compacted)
            }
            AgentCommand::ListModels => self.schedule_agent(
                AgentLane::Main,
                Duration::ZERO,
                AgentEvent::ModelsListed(controlled_models()),
            ),
            AgentCommand::SelectModel(selection) => {
                self.schedule_agent(
                    AgentLane::Main,
                    Duration::ZERO,
                    AgentEvent::ModelSelectionChanged(selection.clone()),
                );
                self.schedule_agent(
                    AgentLane::Main,
                    Duration::ZERO,
                    AgentEvent::ModelChanged(selection.model_id),
                );
            }
            AgentCommand::PruneSessions {
                request_id,
                work_item_ids,
                export_first,
                paths,
            } => {
                self.schedule_agent(
                    AgentLane::Main,
                    Duration::ZERO,
                    AgentEvent::PruneSessionsStarted {
                        request_id: request_id.clone(),
                        work_items: work_item_ids.len(),
                    },
                );
                let state = Arc::clone(&self.state);
                let current_work_item_id = self.work_item_id.clone();
                std::thread::spawn(move || {
                    let outcomes = match (
                        Storage::open(&paths.database),
                        tokio::runtime::Runtime::new(),
                    ) {
                        (Ok(storage), Ok(runtime)) => work_item_ids
                            .into_iter()
                            .map(|work_item_id| {
                                if work_item_id == current_work_item_id {
                                    return PruneSessionOutcome {
                                        work_item_id,
                                        remote_deleted: false,
                                        local_deleted: false,
                                        error: Some(
                                            "the controlled worker refused to prune its open Work Item"
                                                .into(),
                                        ),
                                    };
                                }
                                let progress_request = request_id.clone();
                                let progress_item = work_item_id.clone();
                                let operation_id = format!("{request_id}:{work_item_id}");
                                runtime.block_on(execute_prune_operation(
                                    &ControlledCleanupBackend,
                                    PruneExecution {
                                        ledger: &storage,
                                        database_path: &paths.database,
                                        owner_id: "controlled-prune-owner",
                                        requested_operation_id: &operation_id,
                                        work_item_id: &work_item_id,
                                        export_first,
                                        paths: &paths,
                                    },
                                    |label, completed, total| {
                                        Self::schedule_shared(
                                            &state,
                                            Duration::ZERO,
                                            AgentEventEnvelope::agent(
                                                AgentLane::Main,
                                                AgentEvent::PruneSessionProgress {
                                                    request_id: progress_request.clone(),
                                                    work_item_id: progress_item.clone(),
                                                    label,
                                                    completed,
                                                    total,
                                                },
                                            ),
                                        );
                                    },
                                ))
                            })
                            .collect(),
                        (storage, runtime) => {
                            let error = storage
                                .err()
                                .map(|error| error.to_string())
                                .or_else(|| runtime.err().map(|error| error.to_string()))
                                .unwrap_or_else(|| "controlled prune setup failed".into());
                            work_item_ids
                                .into_iter()
                                .map(|work_item_id| PruneSessionOutcome {
                                    work_item_id,
                                    remote_deleted: false,
                                    local_deleted: false,
                                    error: Some(error.clone()),
                                })
                                .collect()
                        }
                    };
                    Self::schedule_shared(
                        &state,
                        Duration::ZERO,
                        AgentEventEnvelope::agent(
                            AgentLane::Main,
                            AgentEvent::PruneSessionsComplete {
                                request_id,
                                outcomes,
                            },
                        ),
                    );
                });
            }
            AgentCommand::Shutdown => {
                self.schedule_agent(AgentLane::Main, Duration::ZERO, AgentEvent::Stopped)
            }
        }
        Ok(())
    }
}

impl AgentRuntime for ControlledAgent {
    fn try_recv(&self) -> Option<AgentEvent> {
        self.try_recv_laned().map(AgentEventEnvelope::legacy)
    }

    fn try_recv_laned(&self) -> Option<AgentEventEnvelope> {
        let mut state = self.state.lock().expect("controlled agent lock");
        if state
            .events
            .front()
            .is_some_and(|(available_at, _)| *available_at <= Instant::now())
        {
            let event = state.events.pop_front().map(|(_, event)| event)?;
            match &event.event {
                LaneEvent::Agent(AgentEvent::ResponseStarted { outbound_id, .. }) => {
                    state.active_outbound_id = Some(outbound_id.clone());
                }
                LaneEvent::Agent(
                    AgentEvent::ResponseComplete { outbound_id, .. }
                    | AgentEvent::TurnFailed { outbound_id, .. },
                ) => {
                    if state.active_outbound_id.as_deref() == Some(outbound_id.as_str()) {
                        state.active_outbound_id = None;
                    }
                    state.outbound_lanes.remove(outbound_id);
                }
                _ => {}
            }
            return Some(event);
        }
        None
    }
}

impl Drop for CopilotBridge {
    fn drop(&mut self) {
        let _ = self.commands.send(AgentCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn tracked_lane(current_lane: &Arc<std::sync::Mutex<AgentLane>>) -> AgentLane {
    current_lane
        .lock()
        .expect("current Copilot lane lock")
        .clone()
}

fn set_current_lane(current_lane: &Arc<std::sync::Mutex<AgentLane>>, lane: AgentLane) {
    *current_lane.lock().expect("current Copilot lane lock") = lane;
}

fn envelope_outbound_id(envelope: &AgentEventEnvelope) -> Option<String> {
    match &envelope.event {
        LaneEvent::Agent(
            AgentEvent::Queued { outbound_id, .. }
            | AgentEvent::QueueCancelled { outbound_id }
            | AgentEvent::QueueReplaced { outbound_id, .. }
            | AgentEvent::QueueReplaceRejected { outbound_id, .. }
            | AgentEvent::ResponseStarted { outbound_id, .. }
            | AgentEvent::ResponseDelta { outbound_id, .. }
            | AgentEvent::ResponseSnapshot { outbound_id, .. }
            | AgentEvent::ResponseComplete { outbound_id, .. }
            | AgentEvent::TurnFailed { outbound_id, .. },
        ) => Some(outbound_id.clone()),
        LaneEvent::Agent(AgentEvent::Activity { outbound_id, .. }) => outbound_id.clone(),
        _ => None,
    }
}

struct ActiveOutbound {
    outbound: Outbound,
    response_started: bool,
    /// A turn-start boundary must be observed before turn-scoped events are
    /// attributed to this outbound. This quarantines stale events that arrive
    /// after an earlier turn released the local FIFO.
    turn_started: bool,
    turn_id: Option<String>,
    /// Message identifiers acknowledged by `session.send`. The current CLI
    /// exposes these as interaction metadata rather than as the
    /// `user.message` event ID. There can be more than one while an active
    /// turn is steered with immediate delivery, and each is a trusted root for
    /// its descendant event chain.
    sdk_message_ids: HashSet<String>,
    accepted_event_ids: HashSet<String>,
    message_buffers: HashMap<String, String>,
    message_order: Vec<String>,
    emitted_text: String,
    hidden_message_ids: HashSet<String>,
}

fn queue_position(
    queue: &VecDeque<Outbound>,
    active: &Option<ActiveOutbound>,
    active_lane: &AgentLane,
    queue_lane: &AgentLane,
) -> usize {
    queue.len() + usize::from(active.is_some() && active_lane == queue_lane)
}

fn side_lane(side: Option<&SideSession>) -> Option<AgentLane> {
    side.map(|side| AgentLane::Side {
        id: side.id.clone(),
    })
}

fn queued_outbound_lane(
    outbound_id: &str,
    main_queue: &VecDeque<Outbound>,
    side_queue: &VecDeque<Outbound>,
    side_lane: Option<AgentLane>,
) -> Option<AgentLane> {
    if main_queue.iter().any(|outbound| outbound.id == outbound_id) {
        Some(AgentLane::Main)
    } else if side_queue.iter().any(|outbound| outbound.id == outbound_id) {
        // A SIDE request can be buffered before the fork has an ID. MAIN is
        // deliberately used then: it remains visible while SIDE is starting.
        Some(side_lane.unwrap_or(AgentLane::Main))
    } else {
        None
    }
}

fn outbound_lane(
    outbound_id: &str,
    active: &Option<ActiveOutbound>,
    active_lane: &AgentLane,
    main_queue: &VecDeque<Outbound>,
    side_queue: &VecDeque<Outbound>,
    side: Option<&SideSession>,
) -> Option<AgentLane> {
    if active
        .as_ref()
        .is_some_and(|turn| turn.outbound.id == outbound_id)
    {
        Some(active_lane.clone())
    } else {
        queued_outbound_lane(outbound_id, main_queue, side_queue, side_lane(side))
    }
}

trait EventOutput {
    fn emit(&self, event: AgentEvent);

    fn activity(&self, outbound_id: Option<String>, activity: AgentActivity) {
        self.emit(AgentEvent::Activity {
            outbound_id,
            label: activity.label,
        });
    }
}

impl EventOutput for Sender<AgentEvent> {
    fn emit(&self, event: AgentEvent) {
        self.send(event).ok();
    }
}

#[derive(Clone)]
struct EventPublisher {
    events: Sender<AgentEventEnvelope>,
    lane: AgentLane,
}

impl EventPublisher {
    fn new(events: Sender<AgentEventEnvelope>, lane: AgentLane) -> Self {
        Self { events, lane }
    }

    fn on_lane(&self, lane: AgentLane) -> Self {
        Self {
            events: self.events.clone(),
            lane,
        }
    }

    fn lifecycle(&self, event: LaneEvent) {
        self.events
            .send(AgentEventEnvelope {
                lane: self.lane.clone(),
                event,
                activity: None,
            })
            .ok();
    }
}

impl EventOutput for EventPublisher {
    fn emit(&self, event: AgentEvent) {
        self.events
            .send(AgentEventEnvelope::agent(self.lane.clone(), event))
            .ok();
    }

    fn activity(&self, outbound_id: Option<String>, activity: AgentActivity) {
        self.events
            .send(AgentEventEnvelope::activity(
                self.lane.clone(),
                outbound_id,
                activity,
            ))
            .ok();
    }
}

struct ProgressHooks {
    events: EventPublisher,
}

#[async_trait]
impl SessionHooks for ProgressHooks {
    async fn on_hook(&self, event: HookEvent) -> HookOutput {
        let output = match &event {
            HookEvent::ErrorOccurred { input, .. } => {
                HookOutput::ErrorOccurred(ErrorOccurredOutput {
                    suppress_output: None,
                    error_handling: Some(if input.recoverable {
                        "retry".into()
                    } else {
                        "abort".into()
                    }),
                    retry_count: input.recoverable.then_some(1),
                    user_notification: Some(format!(
                        "Copilot {} error: {}",
                        input.error_context, input.error
                    )),
                })
            }
            _ => HookOutput::None,
        };
        let activity = match event {
            HookEvent::PreToolUse { input, .. } => AgentActivity {
                kind: ActivityKind::ToolStart,
                label: format!("Hook: starting {}", input.tool_name),
                tool: Some(input.tool_name),
                detail: Some("Awaiting the separate read-only permission-policy decision".into()),
            },
            HookEvent::PreMcpToolCall { input, .. } => AgentActivity {
                kind: ActivityKind::ToolStart,
                label: format!("Hook: MCP {} / {}", input.server_name, input.tool_name),
                tool: Some(input.tool_name),
                detail: Some(format!("server {}", input.server_name)),
            },
            HookEvent::PostToolUse { input, .. } => AgentActivity {
                kind: ActivityKind::ToolComplete,
                label: format!("Hook: finished {}", input.tool_name),
                tool: Some(input.tool_name),
                detail: None,
            },
            HookEvent::PostToolUseFailure { input, .. } => AgentActivity {
                // PostToolUseFailure is an observation hook. This handler
                // does not return a retry decision, so reporting Retry here
                // would claim work that the SDK never scheduled.
                kind: ActivityKind::Failure,
                label: format!("Hook: {} failed", input.tool_name),
                tool: Some(input.tool_name),
                detail: Some(input.error),
            },
            HookEvent::UserPromptSubmitted { .. } => AgentActivity {
                kind: ActivityKind::Intent,
                label: "Hook: prompt submitted to Copilot".into(),
                tool: None,
                detail: None,
            },
            HookEvent::SessionStart { input, .. } => AgentActivity::other(format!(
                "Hook: session lifecycle started ({})",
                input.source
            )),
            HookEvent::SessionEnd { .. } => AgentActivity::other("Hook: session lifecycle ended"),
            HookEvent::ErrorOccurred { input, .. } => AgentActivity {
                kind: ActivityKind::Retry,
                label: "Hook: Copilot reported an error".into(),
                tool: None,
                detail: Some(input.error),
            },
            _ => AgentActivity::other("Hook: Copilot lifecycle event"),
        };
        self.events.activity(None, activity);
        output
    }
}

fn progress_hooks(events: &EventPublisher) -> Arc<dyn SessionHooks> {
    Arc::new(ProgressHooks {
        events: events.clone(),
    })
}

struct SessionSlot {
    session: Session,
    subscription: github_copilot_sdk::subscription::EventSubscription,
}

struct SideSession {
    operation_id: String,
    id: String,
    parent_id: String,
    slot: SessionSlot,
    boundary_sent: bool,
}

struct EphemeralLeaseGuard<'a> {
    ledger: &'a Storage,
    work_item_id: &'a str,
    owner_id: &'a str,
    owned: Cell<bool>,
}

struct LeaseHeartbeat {
    owned: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

struct WorkItemProcessLock {
    _file: File,
}

impl WorkItemProcessLock {
    fn try_acquire(paths: &AppPaths, work_item_id: &str) -> Result<Option<Self>> {
        let directory = paths.data.join("work-item-locks");
        std::fs::create_dir_all(&directory)?;
        let digest = format!("{:x}", Sha256::digest(work_item_id.as_bytes()));
        let path = directory.join(format!("{}.lock", &digest[..24]));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for WorkItemProcessLock {
    fn drop(&mut self) {
        // Do not rely on descriptor destruction timing here: prune retry can
        // reacquire the same Work Item immediately on another runtime/thread.
        self._file.unlock().ok();
    }
}

impl LeaseHeartbeat {
    fn start(database_path: PathBuf, work_item_id: String, owner_id: String, owned: bool) -> Self {
        let owned = Arc::new(AtomicBool::new(owned));
        let lost = Arc::new(AtomicBool::new(false));
        let worker_owned = Arc::clone(&owned);
        let worker_lost = Arc::clone(&lost);
        let (stop, stopped) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let ledger = match Storage::open(&database_path) {
                Ok(ledger) => ledger,
                Err(_) => {
                    worker_owned.store(false, Ordering::Release);
                    worker_lost.store(true, Ordering::Release);
                    return;
                }
            };
            loop {
                match stopped.recv_timeout(SIDE_LEASE_HEARTBEAT) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                if !worker_owned.load(Ordering::Acquire) {
                    continue;
                }
                match ledger.renew_ephemeral_session_lease(
                    &work_item_id,
                    &owner_id,
                    wall_clock_ms(),
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        worker_owned.store(false, Ordering::Release);
                        worker_lost.store(true, Ordering::Release);
                    }
                    // The local prune phase intentionally holds an IMMEDIATE
                    // transaction as its filesystem fencing lock. Busy/IO
                    // errors are not evidence that another owner took over.
                    Err(_) => {}
                }
            }
        });
        Self {
            owned,
            lost,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn set_owned(&self, owned: bool) {
        self.owned.store(owned, Ordering::Release);
        if owned {
            self.lost.store(false, Ordering::Release);
        }
    }

    fn take_lost(&self) -> bool {
        self.lost.swap(false, Ordering::AcqRel)
    }
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).ok();
        }
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

impl EphemeralLeaseGuard<'_> {
    fn set_owned(&self, owned: bool) {
        self.owned.set(owned);
    }
}

impl Drop for EphemeralLeaseGuard<'_> {
    fn drop(&mut self) {
        if self.owned.get() {
            self.ledger
                .release_ephemeral_session_lease(self.work_item_id, self.owner_id)
                .ok();
        }
    }
}

fn wall_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn side_session_name(operation_id: &str, parent_id: &str) -> String {
    let parent_hash = format!("{:x}", Sha256::digest(parent_id.as_bytes()));
    format!("rq-tui-side:{operation_id}:{}", &parent_hash[..12])
}

fn short_session_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn ephemeral_record(
    config: &BridgeConfig,
    owner_id: &str,
    operation_id: &str,
    parent_id: Option<String>,
    side_id: Option<String>,
    state: &str,
    created_at: String,
) -> EphemeralSessionRecord {
    EphemeralSessionRecord {
        operation_id: operation_id.to_owned(),
        work_item_id: config.work_item_id.clone(),
        owner_id: owner_id.to_owned(),
        parent_id,
        side_id,
        state: state.to_owned(),
        last_error: None,
        created_at,
        updated_at: now(),
    }
}

fn save_ephemeral_record(ledger: &Storage, record: &EphemeralSessionRecord) -> Result<()> {
    if ledger.record_ephemeral_session(record)? {
        Ok(())
    } else {
        anyhow::bail!(
            "SIDE cleanup ownership changed for operation {}",
            record.operation_id
        )
    }
}

fn clear_ephemeral_record(ledger: &Storage, record: &EphemeralSessionRecord) -> Result<()> {
    if ledger.delete_ephemeral_session(&record.operation_id, &record.owner_id)? {
        Ok(())
    } else {
        anyhow::bail!(
            "SIDE cleanup ownership changed before operation {} could be cleared",
            record.operation_id
        )
    }
}

fn retryable_cleanup_state(state: &str) -> bool {
    matches!(state, "cleanup_pending" | "deleting")
}

fn durable_export_choice(existing: Option<&PruneOperation>, requested: bool) -> bool {
    existing
        .map(|operation| operation.export_first)
        .unwrap_or(requested)
}

#[async_trait]
trait SideCleanupBackend {
    async fn reconcile_side_id(
        &self,
        operation_id: &str,
        parent_id: &str,
    ) -> Result<Option<String>>;
    async fn session_exists(&self, session_id: &str) -> Result<bool>;
    async fn delete_session(&self, session_id: &str) -> Result<()>;

    async fn delete_session_if_present(&self, session_id: &str) -> Result<()> {
        if !self.session_exists(session_id).await? {
            return Ok(());
        }
        match self.delete_session(session_id).await {
            Ok(()) => {
                for attempt in 0..3 {
                    if !self.session_exists(session_id).await? {
                        return Ok(());
                    }
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                anyhow::bail!(
                    "Copilot session {} still exists after the delete API returned success",
                    short_session_id(session_id)
                )
            }
            Err(delete_error) => {
                if self.session_exists(session_id).await? {
                    Err(delete_error)
                } else {
                    Ok(())
                }
            }
        }
    }
}

struct ControlledCleanupBackend;

#[async_trait]
impl SideCleanupBackend for ControlledCleanupBackend {
    async fn reconcile_side_id(
        &self,
        _operation_id: &str,
        _parent_id: &str,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    async fn session_exists(&self, _session_id: &str) -> Result<bool> {
        Ok(false)
    }

    async fn delete_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl SideCleanupBackend for Client {
    async fn reconcile_side_id(
        &self,
        operation_id: &str,
        parent_id: &str,
    ) -> Result<Option<String>> {
        let expected_name = side_session_name(operation_id, parent_id);
        let sessions =
            sdk_call("Copilot SIDE reconciliation", self.rpc().sessions().list()).await?;
        let matches = sessions
            .sessions
            .into_iter()
            .filter(|session| {
                let name_matches = session.get("name").and_then(serde_json::Value::as_str)
                    == Some(expected_name.as_str());
                let advertised_parent = session
                    .get("parentSessionId")
                    .or_else(|| session.get("detachedFromSpawningParentSessionId"))
                    .and_then(serde_json::Value::as_str);
                name_matches && advertised_parent.is_none_or(|parent| parent == parent_id)
            })
            .filter_map(|session| {
                session
                    .get("sessionId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(None),
            [session_id] => Ok(Some(session_id.clone())),
            _ => anyhow::bail!(
                "multiple Copilot sessions matched SIDE operation {operation_id}; refusing ambiguous cleanup"
            ),
        }
    }

    async fn session_exists(&self, session_id: &str) -> Result<bool> {
        let id = SessionId::new(session_id);
        Ok(sdk_call(
            "Copilot session existence check",
            self.get_session_metadata(&id),
        )
        .await?
        .is_some())
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        delete_session_with_timeout(self, session_id).await
    }
}

async fn cleanup_ephemeral_record<B: SideCleanupBackend + Sync>(
    backend: &B,
    ledger: &Storage,
    mut record: EphemeralSessionRecord,
) -> (String, Option<String>) {
    let label = record
        .side_id
        .clone()
        .unwrap_or_else(|| record.operation_id.clone());
    if record.side_id.is_none() {
        let Some(parent_id) = record.parent_id.as_deref() else {
            let message =
                "SIDE fork has no durable parent identity; refusing ambiguous cleanup".to_owned();
            record.state = "cleanup_pending".into();
            record.last_error = Some(message.clone());
            record.updated_at = now();
            save_ephemeral_record(ledger, &record).ok();
            return (label, Some(message));
        };
        match backend
            .reconcile_side_id(&record.operation_id, parent_id)
            .await
        {
            Ok(Some(side_id)) => record.side_id = Some(side_id),
            Ok(None) => {
                let message =
                    "SIDE fork outcome is still unresolved; cleanup will retry in the background and on restart"
                        .to_owned();
                record.state = "cleanup_pending".into();
                record.last_error = Some(message.clone());
                record.updated_at = now();
                save_ephemeral_record(ledger, &record).ok();
                return (label, Some(message));
            }
            Err(error) => {
                let message = format!("Could not reconcile an interrupted SIDE fork: {error}");
                record.state = "cleanup_pending".into();
                record.last_error = Some(message.clone());
                record.updated_at = now();
                save_ephemeral_record(ledger, &record).ok();
                return (label, Some(message));
            }
        }
    }

    record.state = "deleting".into();
    record.last_error = None;
    record.updated_at = now();
    if let Err(error) = save_ephemeral_record(ledger, &record) {
        return (
            label,
            Some(format!(
                "Could not durably mark the SIDE session for deletion: {error}"
            )),
        );
    }
    let side_id = record
        .side_id
        .as_deref()
        .expect("reconciled cleanup record has a SIDE id");
    if !matches!(
        ledger.renew_ephemeral_session_lease(
            &record.work_item_id,
            &record.owner_id,
            wall_clock_ms(),
        ),
        Ok(true)
    ) {
        return (
            side_id.to_owned(),
            Some(format!(
                "SIDE {side_id} cleanup ownership changed before deletion; the stale worker did not call the SDK delete API"
            )),
        );
    }
    if let Err(error) = backend.delete_session_if_present(side_id).await {
        let message = format!("Could not delete orphaned SIDE {side_id}: {error}");
        record.state = "cleanup_pending".into();
        record.last_error = Some(message.clone());
        record.updated_at = now();
        save_ephemeral_record(ledger, &record).ok();
        return (side_id.to_owned(), Some(message));
    }
    match clear_ephemeral_record(ledger, &record) {
        Ok(()) => (side_id.to_owned(), None),
        Err(error) => (
            side_id.to_owned(),
            Some(format!(
                "SIDE {side_id} was deleted, but its cleanup ledger could not be cleared: {error}"
            )),
        ),
    }
}

fn copilot_session_state_hint(session_id: &str) -> String {
    let root = env::var_os("COPILOT_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".copilot")));
    root.map(|root| {
        root.join("session-state")
            .join(session_id)
            .display()
            .to_string()
    })
    .unwrap_or_else(|| format!("<COPILOT_HOME>/session-state/{session_id}"))
}

async fn delete_journaled_remote_sessions<B, F>(
    backend: &B,
    ledger: &Storage,
    owner_id: &str,
    operation_id: &str,
    work_item_id: &str,
    mut progress: F,
) -> Result<()>
where
    B: SideCleanupBackend + Sync,
    F: FnMut(String, usize, usize),
{
    let targets = ledger.prune_targets(operation_id)?;
    let total = targets.len();
    let mut completed = targets
        .iter()
        .filter(|target| target.state == "deleted")
        .count();
    progress(
        format!("Loaded {total} durable Copilot session target(s)"),
        completed,
        total,
    );
    for target in targets
        .into_iter()
        .filter(|target| target.state != "deleted")
    {
        if !matches!(
            ledger.renew_ephemeral_session_lease(work_item_id, owner_id, wall_clock_ms()),
            Ok(true)
        ) {
            anyhow::bail!(
                "Work Item cleanup ownership changed; no further SDK deletions were attempted"
            );
        }
        let deletion = if target.kind == "side" {
            let side_operation_id = target
                .side_operation_id
                .as_deref()
                .context("durable SIDE target is missing its operation id")?;
            let record = ledger
                .ephemeral_sessions(work_item_id)?
                .into_iter()
                .find(|record| record.operation_id == side_operation_id);
            if let Some(record) = record {
                let label = record
                    .side_id
                    .clone()
                    .unwrap_or_else(|| format!("SIDE operation {}", record.operation_id));
                progress(format!("Deleting {label}"), completed, total);
                let (session_id, warning) = cleanup_ephemeral_record(backend, ledger, record).await;
                if let Some(warning) = warning {
                    Err(anyhow::anyhow!(warning))
                } else {
                    Ok(Some(session_id))
                }
            } else if let Some(session_id) = target.session_id.as_deref() {
                progress(
                    format!("Deleting SIDE {}", short_session_id(session_id)),
                    completed,
                    total,
                );
                backend
                    .delete_session_if_present(session_id)
                    .await
                    .map(|()| None)
            } else {
                // SIDE ledger rows are cleared only after SDK deletion was
                // confirmed. A missing row with no snapshotted ID therefore
                // represents a crash between that confirmation and this
                // journal target being acknowledged.
                Ok(None)
            }
        } else {
            let session_id = target
                .session_id
                .as_deref()
                .context("durable persistent target is missing its session id")?;
            progress(
                format!("Deleting Copilot session {}", short_session_id(session_id)),
                completed,
                total,
            );
            backend
                .delete_session_if_present(session_id)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "could not delete Copilot session {}: {error}. Local fallback path: {}",
                        short_session_id(session_id),
                        copilot_session_state_hint(session_id)
                    )
                })
                .map(|()| None)
        };
        let reconciled_session_id = match deletion {
            Ok(session_id) => session_id,
            Err(error) => {
                ledger
                    .mark_prune_target_error_if_owned(
                        operation_id,
                        &target.target_key,
                        &error.to_string(),
                        work_item_id,
                        owner_id,
                    )
                    .ok();
                return Err(error);
            }
        };
        if !matches!(
            ledger.renew_ephemeral_session_lease(work_item_id, owner_id, wall_clock_ms()),
            Ok(true)
        ) {
            anyhow::bail!(
                "Work Item cleanup ownership changed while the SDK deletion was in flight"
            );
        }
        if let Some(session_id) = reconciled_session_id {
            if !ledger.update_prune_target_session_id_if_owned(
                operation_id,
                &target.target_key,
                Some(&session_id),
                work_item_id,
                owner_id,
            )? {
                anyhow::bail!("durable SIDE target or cleanup ownership disappeared");
            }
        }
        if !ledger.mark_prune_target_deleted_if_owned(
            operation_id,
            &target.target_key,
            work_item_id,
            owner_id,
        )? {
            anyhow::bail!(
                "durable prune target or cleanup ownership disappeared after SDK deletion"
            );
        }
        completed += 1;
        progress(
            format!("Remote deletion confirmed for {}", target.target_key),
            completed,
            total,
        );
    }
    if !matches!(
        ledger.renew_ephemeral_session_lease(work_item_id, owner_id, wall_clock_ms()),
        Ok(true)
    ) {
        anyhow::bail!("Work Item cleanup ownership changed before local cleanup");
    }
    if !ledger.mark_prune_phase_if_owned(
        operation_id,
        "local_pending",
        None,
        work_item_id,
        owner_id,
    )? {
        anyhow::bail!(
            "durable prune operation or cleanup ownership disappeared before local cleanup"
        );
    }
    Ok(())
}

struct PruneExecution<'a> {
    ledger: &'a Storage,
    database_path: &'a std::path::Path,
    owner_id: &'a str,
    requested_operation_id: &'a str,
    work_item_id: &'a str,
    export_first: bool,
    paths: &'a AppPaths,
}

async fn execute_prune_operation<B, F>(
    backend: &B,
    execution: PruneExecution<'_>,
    mut progress: F,
) -> PruneSessionOutcome
where
    B: SideCleanupBackend + Sync,
    F: FnMut(String, usize, usize),
{
    let PruneExecution {
        ledger,
        database_path,
        owner_id,
        requested_operation_id,
        work_item_id,
        export_first,
        paths,
    } = execution;
    let failure = |error: String| PruneSessionOutcome {
        work_item_id: work_item_id.to_owned(),
        remote_deleted: false,
        local_deleted: false,
        error: Some(error),
    };
    let _process_lock = match WorkItemProcessLock::try_acquire(paths, work_item_id) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return failure(
                "another rq-tui process is using this Work Item; local history was retained".into(),
            )
        }
        Err(error) => {
            return failure(format!(
                "could not acquire the Work Item process lock: {error}"
            ))
        }
    };
    let existing_operation = match ledger.pending_prune_operations() {
        Ok(operations) => operations
            .into_iter()
            .find(|operation| operation.work_item_id == work_item_id),
        Err(error) => return failure(format!("could not inspect the prune journal: {error}")),
    };
    match ledger.work_item_by_id(work_item_id) {
        Ok(None) => {
            let Some(operation) = existing_operation else {
                return PruneSessionOutcome {
                    work_item_id: work_item_id.to_owned(),
                    remote_deleted: false,
                    local_deleted: true,
                    error: Some("local Work Item no longer exists".into()),
                };
            };
            let all_remote_deleted = ledger
                .prune_targets(&operation.operation_id)
                .is_ok_and(|targets| targets.iter().all(|target| target.state == "deleted"));
            if !all_remote_deleted {
                return PruneSessionOutcome {
                    work_item_id: work_item_id.to_owned(),
                    remote_deleted: false,
                    local_deleted: true,
                    error: Some(
                        "local Work Item is missing while remote prune targets remain unresolved"
                            .into(),
                    ),
                };
            }
            return match ledger.clear_prune_operation(&operation.operation_id) {
                Ok(true) => PruneSessionOutcome {
                    work_item_id: work_item_id.to_owned(),
                    remote_deleted: true,
                    local_deleted: true,
                    error: None,
                },
                Ok(false) => PruneSessionOutcome {
                    work_item_id: work_item_id.to_owned(),
                    remote_deleted: true,
                    local_deleted: true,
                    error: Some("completed prune journal disappeared during recovery".into()),
                },
                Err(error) => PruneSessionOutcome {
                    work_item_id: work_item_id.to_owned(),
                    remote_deleted: true,
                    local_deleted: true,
                    error: Some(format!(
                        "local cleanup completed, but its prune journal could not be cleared: {error}"
                    )),
                },
            };
        }
        Ok(Some(_)) => {}
        Err(error) => return failure(format!("could not inspect local Work Item state: {error}")),
    }
    let now_ms = wall_clock_ms();
    let stale_before_ms =
        now_ms.saturating_sub(SIDE_LEASE_TTL.as_millis().try_into().unwrap_or(i64::MAX));
    match ledger.claim_ephemeral_session_lease(work_item_id, owner_id, now_ms, stale_before_ms) {
        Ok(true) => {}
        Ok(false) => {
            let message =
                "another rq-tui process is using this Work Item; local history was retained";
            return failure(message.into());
        }
        Err(error) => {
            let message = format!("could not acquire the Work Item cleanup lease: {error}");
            return failure(message);
        }
    }
    let lease = EphemeralLeaseGuard {
        ledger,
        work_item_id,
        owner_id,
        owned: Cell::new(true),
    };
    let heartbeat = LeaseHeartbeat::start(
        database_path.to_path_buf(),
        work_item_id.to_owned(),
        owner_id.to_owned(),
        true,
    );
    let operation_id =
        match ledger.begin_prune_operation(requested_operation_id, work_item_id, export_first) {
            Ok(operation_id) => operation_id,
            Err(error) => {
                return failure(format!(
                    "could not create the durable prune journal: {error}"
                ))
            }
        };
    let durable_operation = match ledger.prune_operation(&operation_id) {
        Ok(Some(operation)) => operation,
        Ok(None) => {
            return failure("durable prune journal disappeared before cleanup began".into())
        }
        Err(error) => {
            return failure(format!(
                "could not reload the durable prune journal: {error}"
            ))
        }
    };
    let durable_export_first = durable_export_choice(Some(&durable_operation), export_first);
    if let Err(error) = delete_journaled_remote_sessions(
        backend,
        ledger,
        owner_id,
        &operation_id,
        work_item_id,
        &mut progress,
    )
    .await
    {
        ledger
            .mark_prune_phase_if_owned(
                &operation_id,
                "failed",
                Some(&error.to_string()),
                work_item_id,
                owner_id,
            )
            .ok();
        return failure(error.to_string());
    }
    if heartbeat.take_lost() {
        let message =
            "Work Item cleanup lease was lost before local cleanup; the durable journal was retained";
        ledger
            .mark_prune_phase_if_owned(
                &operation_id,
                "failed",
                Some(message),
                work_item_id,
                owner_id,
            )
            .ok();
        return failure(message.into());
    }

    progress("Cleaning local Git, cache, and history state".into(), 0, 0);
    let database_path = database_path.to_path_buf();
    let local_paths = paths.clone();
    let local_id = work_item_id.to_owned();
    let local_owner = owner_id.to_owned();
    let local_operation_id = operation_id.clone();
    let local_result = tokio::task::spawn_blocking(move || {
        let storage = Storage::open(&database_path)?;
        prune_work_item(
            &storage,
            &local_paths,
            &local_id,
            durable_export_first,
            Some(&local_operation_id),
            Some(&local_owner),
        )
    })
    .await;
    let local_failure = match local_result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("local cleanup failed and can be retried: {error}")),
        Err(error) => Some(format!("the local cleanup worker failed: {error}")),
    };
    if let Some(failure_detail) = local_failure {
        let remote_deleted = ledger
            .prune_targets(&operation_id)
            .is_ok_and(|targets| targets.iter().all(|target| target.state == "deleted"));
        let error = if remote_deleted {
            format!("Copilot sessions are gone, but {failure_detail}")
        } else {
            format!(
                "A late Copilot session was captured; local history was retained and remote cleanup will retry: {failure_detail}"
            )
        };
        ledger
            .mark_prune_phase_if_owned(
                &operation_id,
                "failed",
                Some(&error),
                work_item_id,
                owner_id,
            )
            .ok();
        return PruneSessionOutcome {
            work_item_id: work_item_id.to_owned(),
            remote_deleted,
            local_deleted: false,
            error: Some(error),
        };
    }
    drop(heartbeat);
    drop(lease);
    PruneSessionOutcome {
        work_item_id: work_item_id.to_owned(),
        remote_deleted: true,
        local_deleted: true,
        error: None,
    }
}

async fn worker(
    config: BridgeConfig,
    mut commands: UnboundedReceiver<AgentCommand>,
    raw_events: Sender<AgentEventEnvelope>,
    current_lane: Arc<std::sync::Mutex<AgentLane>>,
) -> Result<()> {
    let main_events = EventPublisher::new(raw_events, AgentLane::Main);
    let _process_lock = WorkItemProcessLock::try_acquire(&config.app_paths, &config.work_item_id)?
        .context(
            "another rq-tui process is already using this Work Item; refusing a concurrent session",
        )?;
    let ledger = Storage::open(&config.database_path)
        .context("cannot open the SIDE session cleanup ledger")?;
    if ledger
        .pending_prune_operations()?
        .iter()
        .any(|operation| operation.work_item_id == config.work_item_id)
    {
        anyhow::bail!(
            "this Work Item has an interrupted prune operation and cannot open a new Copilot session; open another Work Item and retry its prune"
        );
    }
    let owner_id = Uuid::new_v4().to_string();
    let now_ms = wall_clock_ms();
    let stale_before_ms =
        now_ms.saturating_sub(SIDE_LEASE_TTL.as_millis().try_into().unwrap_or(i64::MAX));
    let mut has_side_lease = ledger.claim_ephemeral_session_lease(
        &config.work_item_id,
        &owner_id,
        now_ms,
        stale_before_ms,
    )?;
    if !has_side_lease {
        anyhow::bail!(
            "another rq-tui process is already using this Work Item; refusing to create a concurrent Copilot session"
        );
    }
    let lease_guard = EphemeralLeaseGuard {
        ledger: &ledger,
        work_item_id: &config.work_item_id,
        owner_id: &owner_id,
        owned: Cell::new(has_side_lease),
    };
    let lease_heartbeat = LeaseHeartbeat::start(
        config.database_path.clone(),
        config.work_item_id.clone(),
        owner_id.clone(),
        has_side_lease,
    );
    let cli = find_copilot_cli().context(
        "cannot locate the Copilot CLI; set COPILOT_CLI_PATH or install `copilot` on PATH",
    )?;
    let mut options = ClientOptions::default();
    options.program = CliProgram::Path(cli);
    options.working_directory = config.session_root.clone();
    // Only the explicitly configured global/repository plugin directories are
    // allowed into this review session. Ambient marketplace plugins would make
    // the read-only behavior and deterministic tests host-dependent.
    options
        .env
        .push(("COPILOT_PLUGIN_DIR_ONLY".into(), "true".into()));
    let client = sdk_call("Copilot client startup", Client::start(options)).await?;
    for operation in ledger.pending_prune_operations()? {
        if operation.work_item_id == config.work_item_id {
            main_events.activity(
                None,
                AgentActivity::other(
                    "This Work Item has an interrupted prune journal; cleanup is paused while it is open"
                ),
            );
            continue;
        }
        main_events.activity(
            None,
            AgentActivity::other(format!(
                "Resuming interrupted prune for {}…",
                operation.work_item_id
            )),
        );
        let recovery_request = operation.operation_id.clone();
        let recovery_item = operation.work_item_id.clone();
        main_events.emit(AgentEvent::PruneRecoveryStarted {
            operation_id: operation.operation_id.clone(),
            work_item_id: operation.work_item_id.clone(),
        });
        let outcome = execute_prune_operation(
            &client,
            PruneExecution {
                ledger: &ledger,
                database_path: &config.database_path,
                owner_id: &owner_id,
                requested_operation_id: &operation.operation_id,
                work_item_id: &operation.work_item_id,
                export_first: operation.export_first,
                paths: &config.app_paths,
            },
            |label, completed, total| {
                main_events.emit(AgentEvent::PruneSessionProgress {
                    request_id: recovery_request.clone(),
                    work_item_id: recovery_item.clone(),
                    label,
                    completed,
                    total,
                });
            },
        )
        .await;
        main_events.emit(AgentEvent::PruneRecovery {
            operation_id: operation.operation_id,
            outcome,
        });
    }
    let mut orphan_cleanup_results = Vec::new();
    if has_side_lease {
        for record in ledger.ephemeral_sessions(&config.work_item_id)? {
            main_events.activity(
                None,
                AgentActivity::other(format!(
                    "Cleaning interrupted SIDE {}…",
                    short_session_id(record.side_id.as_deref().unwrap_or(&record.operation_id))
                )),
            );
            orphan_cleanup_results.push(cleanup_ephemeral_record(&client, &ledger, record).await);
        }
    }
    let SessionBoot {
        session,
        resumed,
        mut resume_warning,
    } = sdk_call(
        "Copilot session create/resume",
        create_or_resume_session(&client, &config, &main_events, &ledger),
    )
    .await?;
    let mut resumed_active = None;
    if resumed {
        match sdk_call("Copilot history reload", session.get_events()).await {
            Ok(history) => {
                resumed_active = resumed_active_from_history(&history, session.id());
                main_events.emit(AgentEvent::HistoryLoaded(history_entries(&history)));
            }
            Err(error) => {
                let warning = format!(
                    "Session resumed, but its conversation history could not be restored: {error}"
                );
                main_events.activity(None, AgentActivity::other(warning.clone()));
                resume_warning = Some(warning);
            }
        }
    }
    main_events.emit(AgentEvent::SessionReady {
        session_id: session.id().to_string(),
        resumed,
        resume_warning,
    });
    if !has_side_lease {
        main_events.emit(AgentEvent::OrphanSideCleanup {
            session_id: "lease".into(),
            cleanup_warning: Some(
                "Another rq-tui process owns SIDE lifecycle cleanup for this Work Item; MAIN is available, and /side will unlock here when that lease expires"
                    .into(),
            ),
        });
    }
    for (session_id, cleanup_warning) in orphan_cleanup_results {
        main_events.emit(AgentEvent::OrphanSideCleanup {
            session_id,
            cleanup_warning,
        });
    }

    let mut main = SessionSlot {
        subscription: session.subscribe(),
        session,
    };
    let mut side: Option<SideSession> = None;
    let mut side_requested = false;
    let mut active_lane = AgentLane::Main;
    let mut main_queue = VecDeque::new();
    let mut side_queue = VecDeque::new();
    let mut controls = VecDeque::new();
    let mut active = resumed_active;
    let mut lease_tick = tokio::time::interval(SIDE_LEASE_HEARTBEAT);
    lease_tick.tick().await;
    let mut next_cleanup_retry = Instant::now() + SIDE_CLEANUP_RETRY;
    if let Some(active) = &active {
        main_events.activity(
            Some(active.outbound.id.clone()),
            AgentActivity::other(
                "Resumed pending Copilot work; live output remains attached to this turn",
            ),
        );
    }

    loop {
        if active.is_none() {
            if let Some(control) = controls.pop_front() {
                match control {
                    AgentCommand::PruneSessions {
                        request_id,
                        work_item_ids,
                        export_first,
                        paths,
                    } => {
                        main_events.emit(AgentEvent::PruneSessionsStarted {
                            request_id: request_id.clone(),
                            work_items: work_item_ids.len(),
                        });
                        let mut outcomes = Vec::with_capacity(work_item_ids.len());
                        for work_item_id in work_item_ids {
                            if work_item_id == config.work_item_id {
                                outcomes.push(PruneSessionOutcome {
                                    work_item_id,
                                    remote_deleted: false,
                                    local_deleted: false,
                                    error: Some(
                                        "the worker refused to prune its open Work Item; switch Work Items first"
                                            .into(),
                                    ),
                                });
                                continue;
                            }
                            let progress_request = request_id.clone();
                            let progress_item = work_item_id.clone();
                            let operation_id = format!("{request_id}:{work_item_id}");
                            let outcome = execute_prune_operation(
                                &client,
                                PruneExecution {
                                    ledger: &ledger,
                                    database_path: &config.database_path,
                                    owner_id: &owner_id,
                                    requested_operation_id: &operation_id,
                                    work_item_id: &work_item_id,
                                    export_first,
                                    paths: &paths,
                                },
                                |label, completed, total| {
                                    main_events.emit(AgentEvent::PruneSessionProgress {
                                        request_id: progress_request.clone(),
                                        work_item_id: progress_item.clone(),
                                        label,
                                        completed,
                                        total,
                                    });
                                },
                            )
                            .await;
                            outcomes.push(outcome);
                        }
                        main_events.emit(AgentEvent::PruneSessionsComplete {
                            request_id,
                            outcomes,
                        });
                    }
                    AgentCommand::StartSide { outbound } => {
                        if side.is_some() {
                            main_events.activity(
                                None,
                                AgentActivity::other("A SIDE session is already active; exit it before starting another"),
                            );
                        } else if !has_side_lease {
                            side_requested = false;
                            side_queue.clear();
                            main_events.lifecycle(LaneEvent::SideFailed {
                                message: "Cannot start SIDE because another rq-tui process owns SIDE lifecycle cleanup for this Work Item"
                                    .into(),
                            });
                        } else {
                            if !matches!(
                                ledger.renew_ephemeral_session_lease(
                                    &config.work_item_id,
                                    &owner_id,
                                    wall_clock_ms(),
                                ),
                                Ok(true)
                            ) {
                                has_side_lease = false;
                                lease_guard.set_owned(false);
                                lease_heartbeat.set_owned(false);
                                side_requested = false;
                                side_queue.clear();
                                main_events.lifecycle(LaneEvent::SideFailed {
                                    message: "SIDE lifecycle ownership changed before fork; MAIN is unchanged and cleanup remains with the current owner"
                                        .into(),
                                });
                                continue;
                            }
                            let parent_id = main.session.id().to_string();
                            let operation_id = Uuid::new_v4().to_string();
                            let created_at = now();
                            let mut record = ephemeral_record(
                                &config,
                                &owner_id,
                                &operation_id,
                                Some(parent_id.clone()),
                                None,
                                "intent",
                                created_at.clone(),
                            );
                            if let Err(error) = save_ephemeral_record(&ledger, &record) {
                                side_requested = false;
                                side_queue.clear();
                                main_events.lifecycle(LaneEvent::SideFailed {
                                    message: format!(
                                        "Could not durably prepare SIDE cleanup; no fork was created: {error}"
                                    ),
                                });
                                continue;
                            }
                            main_events.activity(
                                None,
                                AgentActivity::other(
                                    "SIDE cleanup intent saved · creating ephemeral fork from MAIN…",
                                ),
                            );
                            match tokio::time::timeout(
                                SDK_CONTROL_TIMEOUT,
                                client.rpc().sessions().fork(SessionsForkRequest {
                                    session_id: SessionId::new(parent_id.clone()),
                                    to_event_id: None,
                                    name: Some(side_session_name(&operation_id, &parent_id)),
                                }),
                            )
                            .await
                            {
                                Ok(Ok(result)) => {
                                    let side_id = result.session_id.to_string();
                                    if !matches!(
                                        ledger.renew_ephemeral_session_lease(
                                            &config.work_item_id,
                                            &owner_id,
                                            wall_clock_ms(),
                                        ),
                                        Ok(true)
                                    ) {
                                        has_side_lease = false;
                                        lease_guard.set_owned(false);
                                        lease_heartbeat.set_owned(false);
                                        side_requested = false;
                                        side_queue.clear();
                                        main_events.lifecycle(LaneEvent::SideFailed {
                                            message: "SIDE fork completed after lifecycle ownership changed; it was not activated and the current owner will reconcile it"
                                                .into(),
                                        });
                                        continue;
                                    }
                                    record.side_id = Some(side_id.clone());
                                    record.state = "opening".into();
                                    record.updated_at = now();
                                    if let Err(error) = save_ephemeral_record(&ledger, &record) {
                                        side_requested = false;
                                        side_queue.clear();
                                        main_events.lifecycle(LaneEvent::SideFailed {
                                            message: format!(
                                                "SIDE cleanup ownership changed before the fork id could be bound: {error}. The stale worker will not delete it; the current owner will reconcile the named fork"
                                            ),
                                        });
                                        continue;
                                    }
                                    let lane = AgentLane::Side {
                                        id: side_id.clone(),
                                    };
                                    let side_events = main_events.on_lane(lane.clone());
                                    let mut resume = resume_config(
                                        SessionId::new(side_id.clone()),
                                        &config,
                                        Some(&side_events),
                                    );
                                    resume.suppress_resume_event = Some(true);
                                    match tokio::time::timeout(
                                        SDK_CONTROL_TIMEOUT,
                                        client.resume_session(resume),
                                    )
                                    .await
                                    {
                                        Ok(Ok(session)) => {
                                            if !matches!(
                                                ledger.renew_ephemeral_session_lease(
                                                    &config.work_item_id,
                                                    &owner_id,
                                                    wall_clock_ms(),
                                                ),
                                                Ok(true)
                                            ) {
                                                has_side_lease = false;
                                                lease_guard.set_owned(false);
                                                lease_heartbeat.set_owned(false);
                                                disconnect_session(&session).await.ok();
                                                side_requested = false;
                                                side_queue.clear();
                                                main_events.lifecycle(LaneEvent::SideFailed {
                                                    message: "SIDE opened after lifecycle ownership changed; it was disconnected without entering the UI and the current owner will clean it"
                                                        .into(),
                                                });
                                                continue;
                                            }
                                            record.state = "active".into();
                                            record.last_error = None;
                                            record.updated_at = now();
                                            if let Err(error) =
                                                save_ephemeral_record(&ledger, &record)
                                            {
                                                disconnect_session(&session).await.ok();
                                                side_requested = false;
                                                side_queue.clear();
                                                main_events.lifecycle(LaneEvent::SideFailed {
                                                    message: format!(
                                                        "SIDE was disconnected because activation ownership changed: {error}. The stale worker will not delete it; the current owner retains cleanup responsibility"
                                                    ),
                                                });
                                                continue;
                                            }
                                            if let Some(outbound) = outbound {
                                                side_queue.push_front(outbound);
                                            }
                                            let boundary_sent =
                                                side_queue.front_mut().is_some_and(|outbound| {
                                                    apply_side_boundary(outbound);
                                                    true
                                                });
                                            side = Some(SideSession {
                                                operation_id,
                                                id: side_id.clone(),
                                                parent_id: parent_id.clone(),
                                                slot: SessionSlot {
                                                    subscription: session.subscribe(),
                                                    session,
                                                },
                                                boundary_sent,
                                            });
                                            active_lane = lane;
                                            set_current_lane(&current_lane, active_lane.clone());
                                            side_events.lifecycle(LaneEvent::SideStarted {
                                                parent_id,
                                                side_id: side_id.clone(),
                                            });
                                            side_events.activity(
                                                None,
                                                AgentActivity::other(
                                                    "SIDE ready; MAIN history is unchanged",
                                                ),
                                            );
                                            for (position, outbound) in
                                                side_queue.iter().enumerate()
                                            {
                                                side_events.emit(AgentEvent::Queued {
                                                    outbound_id: outbound.id.clone(),
                                                    position,
                                                });
                                            }
                                        }
                                        Ok(Err(error)) => {
                                            side_requested = false;
                                            side_queue.clear();
                                            let (_, cleanup_warning) =
                                                cleanup_ephemeral_record(&client, &ledger, record)
                                                    .await;
                                            main_events.lifecycle(LaneEvent::SideFailed {
                                                message: cleanup_warning.map_or_else(
                                                    || format!(
                                                        "SIDE fork could not be opened and was deleted: {error}"
                                                    ),
                                                    |cleanup| format!(
                                                        "SIDE fork could not be opened: {error}. {cleanup}"
                                                    ),
                                                ),
                                            })
                                        }
                                        Err(_) => {
                                            side_requested = false;
                                            side_queue.clear();
                                            let (_, cleanup_warning) =
                                                cleanup_ephemeral_record(&client, &ledger, record)
                                                    .await;
                                            main_events.lifecycle(LaneEvent::SideFailed {
                                                message: cleanup_warning.map_or_else(
                                                    || format!(
                                                        "SIDE fork {side_id} opening exceeded {} seconds; the fork was deleted",
                                                        SDK_CONTROL_TIMEOUT.as_secs()
                                                    ),
                                                    |cleanup| format!(
                                                        "SIDE fork {side_id} opening exceeded {} seconds. {cleanup}",
                                                        SDK_CONTROL_TIMEOUT.as_secs()
                                                    ),
                                                ),
                                            })
                                        }
                                    }
                                }
                                Ok(Err(error)) => {
                                    side_requested = false;
                                    side_queue.clear();
                                    record.state = "cleanup_pending".into();
                                    record.last_error = Some(format!(
                                        "Fork RPC returned an error before confirming whether SIDE was created: {error}"
                                    ));
                                    record.updated_at = now();
                                    save_ephemeral_record(&ledger, &record)?;
                                    let (_, cleanup_warning) =
                                        cleanup_ephemeral_record(&client, &ledger, record).await;
                                    main_events.lifecycle(LaneEvent::SideFailed {
                                        message: cleanup_warning.map_or_else(
                                            || format!(
                                                "SIDE fork returned an error and the named fork was deleted: {error}"
                                            ),
                                            |cleanup| format!(
                                                "Could not confirm SIDE creation: {error}. {cleanup}"
                                            ),
                                        ),
                                    })
                                }
                                Err(_) => {
                                    side_requested = false;
                                    side_queue.clear();
                                    record.state = "cleanup_pending".into();
                                    record.last_error = Some(
                                        "Fork RPC timed out before returning a SIDE id".into(),
                                    );
                                    record.updated_at = now();
                                    save_ephemeral_record(&ledger, &record)?;
                                    let (_, cleanup_warning) =
                                        cleanup_ephemeral_record(&client, &ledger, record).await;
                                    main_events.lifecycle(LaneEvent::SideFailed {
                                        message: cleanup_warning.map_or_else(
                                            || format!(
                                                "SIDE creation exceeded {} seconds; no fork remained after reconciliation",
                                                SDK_CONTROL_TIMEOUT.as_secs()
                                            ),
                                            |cleanup| format!(
                                                "SIDE creation exceeded {} seconds. {cleanup}",
                                                SDK_CONTROL_TIMEOUT.as_secs()
                                            ),
                                        ),
                                    })
                                }
                            }
                        }
                    }
                    AgentCommand::ExitSide => {
                        if let Some(side_session) = side.take() {
                            let lane = AgentLane::Side {
                                id: side_session.id.clone(),
                            };
                            let side_events = main_events.on_lane(lane);
                            let side_id = side_session.id.clone();
                            let mut cleanup_record = ephemeral_record(
                                &config,
                                &owner_id,
                                &side_session.operation_id,
                                Some(side_session.parent_id.clone()),
                                Some(side_id.clone()),
                                "cleanup_pending",
                                now(),
                            );
                            if let Some(existing) = ledger
                                .ephemeral_sessions(&config.work_item_id)?
                                .into_iter()
                                .find(|record| record.operation_id == side_session.operation_id)
                            {
                                cleanup_record.created_at = existing.created_at;
                            }
                            let prepare_warning = save_ephemeral_record(&ledger, &cleanup_record)
                                .err()
                                .map(|error| {
                                    format!(
                                        "Could not durably mark SIDE cleanup as pending: {error}"
                                    )
                                });
                            disconnect_session(&side_session.slot.session).await.ok();
                            let (_, delete_warning) =
                                cleanup_ephemeral_record(&client, &ledger, cleanup_record).await;
                            let cleanup_warning = match (prepare_warning, delete_warning) {
                                (Some(prepare), Some(delete)) => {
                                    Some(format!("{prepare}. {delete}"))
                                }
                                (Some(warning), None) | (None, Some(warning)) => Some(warning),
                                (None, None) => None,
                            };
                            side_events.lifecycle(LaneEvent::SideExited {
                                parent_id: side_session.parent_id,
                                side_id,
                                cleanup_warning,
                            });
                            side_queue.clear();
                            side_requested = false;
                            active_lane = AgentLane::Main;
                            set_current_lane(&current_lane, active_lane.clone());
                        } else {
                            side_requested = false;
                            side_queue.clear();
                            main_events
                                .activity(None, AgentActivity::other("No SIDE session is active"));
                        }
                    }
                    AgentCommand::SelectModel(selection) => {
                        let events = main_events.on_lane(active_lane.clone());
                        let slot = if let Some(side) =
                            side.as_mut().filter(|_| active_lane != AgentLane::Main)
                        {
                            &mut side.slot
                        } else {
                            &mut main
                        };
                        match set_model_options(&selection) {
                            Ok(options) => match sdk_call(
                                "Copilot model selection",
                                slot.session.set_model(&selection.model_id, Some(options)),
                            )
                            .await
                            {
                                Ok(()) => {
                                    events
                                        .emit(AgentEvent::ModelSelectionChanged(selection.clone()));
                                    events.emit(AgentEvent::ModelChanged(selection.model_id));
                                }
                                Err(error) => events.emit(AgentEvent::ModelSelectionFailed {
                                    selection: selection.clone(),
                                    message: error.to_string(),
                                }),
                            },
                            Err(error) => events.emit(AgentEvent::ModelSelectionFailed {
                                selection: selection.clone(),
                                message: error.to_string(),
                            }),
                        }
                    }
                    AgentCommand::Compact(instructions) => {
                        let events = main_events.on_lane(active_lane.clone());
                        let slot = if let Some(side) =
                            side.as_mut().filter(|_| active_lane != AgentLane::Main)
                        {
                            &mut side.slot
                        } else {
                            &mut main
                        };
                        let result = match instructions {
                            Some(custom_instructions) => sdk_call(
                                "Copilot history compaction",
                                slot.session.rpc().history().compact_with_params(
                                    HistoryCompactRequest {
                                        custom_instructions: Some(custom_instructions),
                                    },
                                ),
                            )
                            .await
                            .map(|_| ()),
                            None => sdk_call(
                                "Copilot history compaction",
                                slot.session.rpc().history().compact(),
                            )
                            .await
                            .map(|_| ()),
                        };
                        match result {
                            Ok(()) => events.emit(AgentEvent::Compacted),
                            Err(error) => events.activity(
                                None,
                                AgentActivity::other(format!("Could not compact session: {error}")),
                            ),
                        }
                    }
                    AgentCommand::Fork => {
                        if active_lane != AgentLane::Main {
                            main_events.on_lane(active_lane.clone()).activity(
                                None,
                                AgentActivity::other(
                                    "Exit SIDE before replacing the persisted MAIN fork",
                                ),
                            );
                            continue;
                        }
                        let parent_id = main.session.id().to_string();
                        if !matches!(
                            ledger.renew_ephemeral_session_lease(
                                &config.work_item_id,
                                &owner_id,
                                wall_clock_ms(),
                            ),
                            Ok(true)
                        ) {
                            main_events.activity(
                                None,
                                AgentActivity::other(
                                    "Cannot fork MAIN because Work Item ownership changed",
                                ),
                            );
                            continue;
                        }
                        let operation_id = Uuid::new_v4().to_string();
                        let created_at = now();
                        let mut fork_intent = ephemeral_record(
                            &config,
                            &owner_id,
                            &operation_id,
                            Some(parent_id.clone()),
                            None,
                            "intent",
                            created_at,
                        );
                        if let Err(error) = save_ephemeral_record(&ledger, &fork_intent) {
                            main_events.activity(
                                None,
                                AgentActivity::other(format!(
                                    "Could not journal MAIN fork intent: {error}"
                                )),
                            );
                            continue;
                        }
                        match sdk_call(
                            "Copilot MAIN fork",
                            client.rpc().sessions().fork(SessionsForkRequest {
                                session_id: SessionId::new(parent_id.clone()),
                                to_event_id: None,
                                name: Some(side_session_name(&operation_id, &parent_id)),
                            }),
                        )
                        .await
                        {
                            Ok(result) => {
                                let new_id = result.session_id.to_string();
                                fork_intent.side_id = Some(new_id.clone());
                                fork_intent.state = "opening".into();
                                fork_intent.updated_at = now();
                                if let Err(error) = save_ephemeral_record(&ledger, &fork_intent) {
                                    let (_, cleanup_warning) =
                                        cleanup_ephemeral_record(&client, &ledger, fork_intent)
                                            .await;
                                    main_events.activity(
                                        None,
                                        AgentActivity::other(format!(
                                            "MAIN fork ownership changed before activation: {error}{}",
                                            cleanup_warning
                                                .map(|warning| format!("; {warning}"))
                                                .unwrap_or_default()
                                        )),
                                    );
                                    continue;
                                }
                                let mut resume = resume_config(
                                    SessionId::new(new_id.clone()),
                                    &config,
                                    Some(&main_events),
                                );
                                resume.suppress_resume_event = Some(true);
                                match sdk_call(
                                    "Copilot MAIN fork activation",
                                    client.resume_session(resume),
                                )
                                .await
                                {
                                    Ok(session) => {
                                        let activation = ledger.activate_session_from_ephemeral(
                                            &SessionRecord {
                                                id: new_id.clone(),
                                                work_item_id: config.work_item_id.clone(),
                                                parent_id: Some(parent_id.clone()),
                                                active: true,
                                                created_at: now(),
                                            },
                                            &operation_id,
                                            &owner_id,
                                        );
                                        if !matches!(activation, Ok(true)) {
                                            disconnect_session(&session).await.ok();
                                            fork_intent.state = "cleanup_pending".into();
                                            fork_intent.last_error =
                                                activation.err().map(|error| error.to_string());
                                            fork_intent.updated_at = now();
                                            save_ephemeral_record(&ledger, &fork_intent).ok();
                                            let (_, cleanup_warning) = cleanup_ephemeral_record(
                                                &client,
                                                &ledger,
                                                fork_intent,
                                            )
                                            .await;
                                            let cleanup = cleanup_warning
                                                .map(|warning| format!("; {warning}"))
                                                .unwrap_or_default();
                                            main_events.activity(
                                                None,
                                                AgentActivity::other(format!(
                                                    "Refused an unfenced MAIN fork{cleanup}"
                                                )),
                                            );
                                        } else {
                                            disconnect_session(&main.session).await.ok();
                                            main = SessionSlot {
                                                subscription: session.subscribe(),
                                                session,
                                            };
                                            main_events.emit(AgentEvent::Forked {
                                                parent_id,
                                                session_id: new_id,
                                            });
                                        }
                                    }
                                    Err(error) => {
                                        fork_intent.state = "cleanup_pending".into();
                                        fork_intent.last_error = Some(error.to_string());
                                        fork_intent.updated_at = now();
                                        save_ephemeral_record(&ledger, &fork_intent).ok();
                                        let (_, cleanup_warning) =
                                            cleanup_ephemeral_record(&client, &ledger, fork_intent)
                                                .await;
                                        main_events.activity(
                                            None,
                                            AgentActivity::other(format!(
                                                "Fork was created but could not be activated: {error}{}",
                                                cleanup_warning
                                                    .map(|warning| format!("; {warning}"))
                                                    .unwrap_or_default()
                                            )),
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                fork_intent.state = "cleanup_pending".into();
                                fork_intent.last_error = Some(error.to_string());
                                fork_intent.updated_at = now();
                                // The RPC can fail after the remote side commits.
                                // Retain the name-bound intent so startup can
                                // reconcile and delete that possible orphan.
                                save_ephemeral_record(&ledger, &fork_intent).ok();
                                let (_, cleanup_warning) =
                                    cleanup_ephemeral_record(&client, &ledger, fork_intent).await;
                                main_events.activity(
                                    None,
                                    AgentActivity::other(format!(
                                        "Could not fork session: {error}{}",
                                        cleanup_warning
                                            .map(|warning| format!("; {warning}"))
                                            .unwrap_or_default()
                                    )),
                                );
                            }
                        }
                    }
                    AgentCommand::Send(_)
                    | AgentCommand::Steer(_)
                    | AgentCommand::CancelQueued(_)
                    | AgentCommand::ReplaceQueued { .. }
                    | AgentCommand::SendSide(_)
                    | AgentCommand::CancelSide
                    | AgentCommand::Abort
                    | AgentCommand::ListModels
                    | AgentCommand::Shutdown => {}
                }
                // A control may switch lanes or enqueue the first SIDE turn.
                // Re-enter the top of the loop so that turn starts immediately
                // instead of waiting for an unrelated SDK event.
                continue;
            } else {
                let (slot, queue, events) =
                    if let Some(side) = side.as_mut().filter(|_| active_lane != AgentLane::Main) {
                        (
                            &side.slot.session,
                            &mut side_queue,
                            main_events.on_lane(active_lane.clone()),
                        )
                    } else {
                        (&main.session, &mut main_queue, main_events.clone())
                    };
                start_next(slot, queue, &mut active, &events).await;
            }
        }
        tokio::select! {
            _ = lease_tick.tick() => {
                if has_side_lease {
                    if lease_heartbeat.take_lost() {
                        has_side_lease = false;
                        lease_guard.set_owned(false);
                        lease_heartbeat.set_owned(false);
                        if let Some(side_session) = side.take() {
                            let lane = AgentLane::Side {
                                id: side_session.id.clone(),
                            };
                            abort_session(&side_session.slot.session).await.ok();
                            disconnect_session(&side_session.slot.session).await.ok();
                            let message = "SIDE closed because this process lost its lifecycle lease; its durable cleanup record was retained for the current owner";
                            fail_active(
                                &mut active,
                                message.into(),
                                &main_events.on_lane(lane.clone()),
                            );
                            fail_queue(
                                &mut side_queue,
                                message,
                                &main_events.on_lane(lane.clone()),
                            );
                            main_events.on_lane(lane).lifecycle(LaneEvent::SideExited {
                                parent_id: side_session.parent_id,
                                side_id: side_session.id,
                                cleanup_warning: Some(message.into()),
                            });
                            active_lane = AgentLane::Main;
                            set_current_lane(&current_lane, active_lane.clone());
                        }
                        side_requested = false;
                        main_events.activity(
                            None,
                            AgentActivity::other(
                                "SIDE lifecycle lease lost · MAIN remains available · /side will unlock after lease recovery",
                            ),
                        );
                    } else if Instant::now() >= next_cleanup_retry
                        && active.is_none()
                        && main_queue.is_empty()
                        && side.is_none()
                    {
                        next_cleanup_retry = Instant::now() + SIDE_CLEANUP_RETRY;
                        let pending_cleanup = ledger
                            .ephemeral_sessions(&config.work_item_id)?
                            .into_iter()
                            .filter(|record| retryable_cleanup_state(&record.state))
                            .collect::<Vec<_>>();
                        if !pending_cleanup.is_empty() {
                            main_events.activity(
                                None,
                                AgentActivity::other(format!(
                                    "Retrying {} interrupted SIDE cleanup operation(s)…",
                                    pending_cleanup.len()
                                )),
                            );
                        }
                        for record in pending_cleanup {
                            let (session_id, cleanup_warning) =
                                cleanup_ephemeral_record(&client, &ledger, record).await;
                            main_events.emit(AgentEvent::OrphanSideCleanup {
                                session_id,
                                cleanup_warning,
                            });
                        }
                    }
                } else {
                    let now_ms = wall_clock_ms();
                    let stale_before_ms = now_ms.saturating_sub(
                        SIDE_LEASE_TTL
                            .as_millis()
                            .try_into()
                            .unwrap_or(i64::MAX),
                    );
                    if ledger.claim_ephemeral_session_lease(
                        &config.work_item_id,
                        &owner_id,
                        now_ms,
                        stale_before_ms,
                    )? {
                        has_side_lease = true;
                        lease_guard.set_owned(true);
                        lease_heartbeat.set_owned(true);
                        next_cleanup_retry = Instant::now() + SIDE_CLEANUP_RETRY;
                        main_events.activity(
                            None,
                            AgentActivity::other(
                                "SIDE lifecycle lease acquired · reconciling interrupted sessions…",
                            ),
                        );
                        let mut cleanup_had_warning = false;
                        for record in ledger.ephemeral_sessions(&config.work_item_id)? {
                            let (session_id, cleanup_warning) =
                                cleanup_ephemeral_record(&client, &ledger, record).await;
                            cleanup_had_warning |= cleanup_warning.is_some();
                            main_events.emit(AgentEvent::OrphanSideCleanup {
                                session_id,
                                cleanup_warning,
                            });
                        }
                        if !cleanup_had_warning {
                            main_events.activity(
                                None,
                                AgentActivity::other(
                                    "SIDE lifecycle ready · /side is available",
                                ),
                            );
                        }
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    AgentCommand::ListModels => {
                        let events = main_events.on_lane(active_lane.clone());
                        match sdk_call("Copilot model listing", client.list_models()).await {
                            Ok(models) => events.emit(AgentEvent::ModelsListed(
                                models.iter().map(model_option).collect(),
                            )),
                            Err(error) => events.activity(
                                None,
                                AgentActivity::other(format!(
                                    "Could not list Copilot models: {error}"
                                )),
                            ),
                        }
                    }
                    AgentCommand::Send(outbound) => {
                        let position = queue_position(
                            &main_queue,
                            &active,
                            &active_lane,
                            &AgentLane::Main,
                        );
                        let outbound_id = outbound.id.clone();
                        main_queue.push_back(outbound);
                        main_events.emit(AgentEvent::Queued { outbound_id, position });
                    }
                    AgentCommand::Steer(outbound) => {
                        let steering_id = outbound.id.clone();
                        let events = main_events.on_lane(active_lane.clone());
                        if active.is_none() {
                            let queue = if active_lane == AgentLane::Main {
                                &mut main_queue
                            } else {
                                &mut side_queue
                            };
                            let position = queue_position(queue, &active, &active_lane, &active_lane);
                            queue.push_back(outbound);
                            events.emit(AgentEvent::Queued {
                                outbound_id: steering_id,
                                position,
                            });
                        } else {
                            let slot = if let Some(side) =
                                side.as_mut().filter(|_| active_lane != AgentLane::Main)
                            {
                                &mut side.slot
                            } else {
                                &mut main
                            };
                            let outbound_id =
                                active.as_ref().map(|turn| turn.outbound.id.clone());
                            match sdk_call(
                                "Copilot immediate steering",
                                slot.session.send(
                                    MessageOptions::new(outbound.text.clone())
                                        .with_mode(DeliveryMode::Immediate),
                                ),
                            )
                            .await
                            {
                                Ok(message_id) => {
                                    if let Some(active) = active.as_mut() {
                                        register_sdk_message_root(active, message_id);
                                    }
                                    events.emit(AgentEvent::SteeringAccepted {
                                        steering_id,
                                        active_outbound_id: outbound_id
                                            .expect("active steering has an outbound id"),
                                    });
                                }
                                Err(error) => events.emit(AgentEvent::SteeringFailed {
                                    steering_id,
                                    active_outbound_id: outbound_id
                                        .expect("active steering has an outbound id"),
                                    message: error.to_string(),
                                }),
                            }
                        }
                    }
                    AgentCommand::CancelQueued(outbound_id) => {
                        let owner = queued_outbound_lane(
                            &outbound_id,
                            &main_queue,
                            &side_queue,
                            side_lane(side.as_ref()),
                        );
                        if let Some(owner) = owner {
                            main_queue.retain(|outbound| outbound.id != outbound_id);
                            side_queue.retain(|outbound| outbound.id != outbound_id);
                            main_events.on_lane(owner).emit(AgentEvent::QueueCancelled { outbound_id });
                        } else {
                            let owner = outbound_lane(
                                &outbound_id,
                                &active,
                                &active_lane,
                                &main_queue,
                                &side_queue,
                                side.as_ref(),
                            )
                            .unwrap_or_else(|| active_lane.clone());
                            let events = main_events.on_lane(owner);
                            let activity_id = active
                                .as_ref()
                                .filter(|turn| turn.outbound.id == outbound_id)
                                .map(|turn| turn.outbound.id.clone());
                            events.activity(
                                activity_id,
                                AgentActivity::other(
                                    "Prompt was already active or no longer queued",
                                ),
                            );
                        }
                    }
                    AgentCommand::ReplaceQueued {
                        outbound_id,
                        replacement,
                    } => {
                        let replacement_id = replacement.id.clone();
                        if let Some(position) = main_queue
                            .iter()
                            .position(|queued| queued.id == outbound_id)
                        {
                            main_queue[position] = replacement;
                            main_events.emit(AgentEvent::QueueReplaced {
                                outbound_id,
                                replacement_id,
                                position: position
                                    + usize::from(
                                        active_lane == AgentLane::Main && active.is_some(),
                                    ),
                            });
                        } else if let Some(position) = side_queue
                            .iter()
                            .position(|queued| queued.id == outbound_id)
                        {
                            side_queue[position] = replacement;
                            let lane = side
                                .as_ref()
                                .map(|side| AgentLane::Side {
                                    id: side.id.clone(),
                                })
                                .unwrap_or_else(|| active_lane.clone());
                            main_events.on_lane(lane).emit(AgentEvent::QueueReplaced {
                                outbound_id,
                                replacement_id,
                                position: position
                                    + usize::from(active_lane != AgentLane::Main && active.is_some()),
                            });
                        } else {
                            let original_active = active
                                .as_ref()
                                .is_some_and(|turn| turn.outbound.id == outbound_id);
                            let owner = outbound_lane(
                                &outbound_id,
                                &active,
                                &active_lane,
                                &main_queue,
                                &side_queue,
                                side.as_ref(),
                            )
                            .unwrap_or_else(|| active_lane.clone());
                            main_events.on_lane(owner).emit(
                                AgentEvent::QueueReplaceRejected {
                                    outbound_id,
                                    replacement_id,
                                    original_active,
                                    reason: if original_active {
                                        "prompt already started".into()
                                    } else {
                                        "prompt already left the queue".into()
                                    },
                                },
                            );
                        }
                    }
                    AgentCommand::SendSide(mut outbound) => {
                        if let Some(side_session) = side.as_mut() {
                            if !side_session.boundary_sent {
                                apply_side_boundary(&mut outbound);
                                side_session.boundary_sent = true;
                            }
                            let lane = AgentLane::Side {
                                id: side_session.id.clone(),
                            };
                            let position = queue_position(
                                &side_queue,
                                &active,
                                &active_lane,
                                &lane,
                            );
                            let outbound_id = outbound.id.clone();
                            side_queue.push_back(outbound);
                            main_events
                                .on_lane(lane)
                                .emit(AgentEvent::Queued { outbound_id, position });
                        } else if side_requested {
                            side_queue.push_back(outbound);
                            main_events.activity(
                                None,
                                AgentActivity::other(
                                    "SIDE message buffered until the ephemeral fork is ready",
                                ),
                            );
                        } else {
                            main_events.activity(None, AgentActivity::other("No SIDE session is active; use /side first"));
                        }
                    }
                    AgentCommand::CancelSide => {
                        if side.is_none() {
                            controls.retain(|control| {
                                !matches!(control, AgentCommand::StartSide { .. })
                            });
                            side_queue.clear();
                            side_requested = false;
                            main_events.lifecycle(LaneEvent::SideCancelled);
                        } else {
                            if active.is_some() && active_lane != AgentLane::Main {
                                if let Some(side_session) = side.as_mut() {
                                    abort_session(&side_session.slot.session).await.ok();
                                }
                            }
                            controls.push_front(AgentCommand::ExitSide);
                            main_events.on_lane(active_lane.clone()).activity(
                                active.as_ref().map(|turn| turn.outbound.id.clone()),
                                AgentActivity::other(
                                    "Cancelling SIDE creation and returning to MAIN…",
                                ),
                            );
                        }
                    }
                    AgentCommand::Abort => {
                        if active.is_some() {
                            let slot = if let Some(side) = side.as_mut().filter(|_| active_lane != AgentLane::Main) {
                                &mut side.slot
                            } else {
                                &mut main
                            };
                            match abort_session(&slot.session).await {
                                Ok(()) => {
                                    main_events.on_lane(active_lane.clone()).activity(
                                        active.as_ref().map(|turn| turn.outbound.id.clone()),
                                        AgentActivity::other("Stopping the current response…"),
                                    );
                                }
                                Err(error) => {
                                    let events = main_events.on_lane(active_lane.clone());
                                    fail_active(&mut active, error.to_string(), &events);
                                }
                            }
                        } else {
                            main_events
                                .on_lane(active_lane.clone())
                                .emit(AgentEvent::StopSettledAlreadyIdle);
                        }
                    }
                    AgentCommand::StartSide { .. } => {
                        side_requested = true;
                        controls.push_back(command);
                        main_events.on_lane(active_lane.clone()).activity(
                            active.as_ref().map(|turn| turn.outbound.id.clone()),
                            AgentActivity::other(
                                "SIDE creation queued after the current response",
                            ),
                        );
                    }
                    AgentCommand::ExitSide => {
                        if active.is_some() && active_lane != AgentLane::Main {
                            if let Some(side_session) = side.as_mut() {
                                match abort_session(&side_session.slot.session).await {
                                    Ok(()) => {
                                        main_events.on_lane(active_lane.clone()).activity(
                                            active
                                                .as_ref()
                                                .map(|turn| turn.outbound.id.clone()),
                                            AgentActivity::other(
                                                "Stopping the SIDE response before returning to MAIN…",
                                            ),
                                        );
                                    }
                                    Err(error) => {
                                        let events =
                                            main_events.on_lane(active_lane.clone());
                                        fail_active(
                                            &mut active,
                                            format!(
                                                "Could not stop SIDE before returning to MAIN: {error}"
                                            ),
                                            &events,
                                        );
                                    }
                                }
                            }
                            controls.push_front(AgentCommand::ExitSide);
                        } else {
                            controls.push_front(AgentCommand::ExitSide);
                        }
                    }
                    AgentCommand::PruneSessions { .. } => {
                        controls.push_back(command);
                        main_events.activity(
                            active.as_ref().map(|turn| turn.outbound.id.clone()),
                            AgentActivity::other(if active.is_some() {
                                "Work Item prune queued until the current response is idle"
                            } else {
                                "Starting Work Item session cleanup…"
                            }),
                        );
                    }
                    AgentCommand::Fork
                    | AgentCommand::Compact(_)
                    | AgentCommand::SelectModel(_) => {
                        let label = match &command {
                            AgentCommand::Fork => "Fork queued after the current response",
                            AgentCommand::Compact(_) => "Compaction queued after the current response",
                            AgentCommand::SelectModel(_) => {
                                "Model selection queued after the current response"
                            }
                            _ => unreachable!(),
                        };
                        controls.push_back(command);
                        main_events.on_lane(active_lane.clone()).activity(
                            active.as_ref().map(|turn| turn.outbound.id.clone()),
                            AgentActivity::other(label),
                        );
                    }
                    AgentCommand::Shutdown => {
                        if active.is_some() {
                            let slot = if let Some(side) = side.as_mut().filter(|_| active_lane != AgentLane::Main) {
                                &mut side.slot
                            } else {
                                &mut main
                            };
                            abort_session(&slot.session).await.ok();
                        }
                        break;
                    },
                }
            }
            incoming = async {
                if let Some(side) = side.as_mut().filter(|_| active_lane != AgentLane::Main) {
                    side.slot.subscription.recv().await
                } else {
                    main.subscription.recv().await
                }
            } => {
                match incoming {
                    Ok(event) => {
                        let events = main_events.on_lane(active_lane.clone());
                        handle_session_event(event, &mut active, &events);
                    }
                    Err(error) => {
                        match error.kind() {
                            RecvErrorKind::Lagged(lagged) => {
                                let message = format!(
                                    "Copilot event stream skipped {} event(s); the worker stopped safely so an idle boundary cannot be lost. Restart rq-tui to resume the persisted session",
                                    lagged.skipped()
                                );
                                let current_events = main_events.on_lane(active_lane.clone());
                                fail_active(&mut active, message.clone(), &current_events);
                                fail_queue(&mut main_queue, &message, &main_events);
                                if let Some(side_session) = &side {
                                    fail_queue(
                                        &mut side_queue,
                                        &message,
                                        &main_events.on_lane(AgentLane::Side {
                                            id: side_session.id.clone(),
                                        }),
                                    );
                                } else {
                                    fail_queue(&mut side_queue, &message, &current_events);
                                }
                                current_events.emit(AgentEvent::Error(message));
                                break;
                            }
                            RecvErrorKind::Closed => {
                                let message = format!(
                                    "Copilot event stream closed: {error}. Queued prompts were retained as failed/recoverable UI entries; restart rq-tui to resume"
                                );
                                let current_events = main_events.on_lane(active_lane.clone());
                                fail_active(&mut active, message.clone(), &current_events);
                                fail_queue(&mut main_queue, &message, &main_events);
                                if let Some(side_session) = &side {
                                    fail_queue(
                                        &mut side_queue,
                                        &message,
                                        &main_events.on_lane(AgentLane::Side {
                                            id: side_session.id.clone(),
                                        }),
                                    );
                                } else {
                                    fail_queue(&mut side_queue, &message, &current_events);
                                }
                                current_events.emit(AgentEvent::Error(message));
                                break;
                            }
                            _ => {
                                main_events.on_lane(active_lane.clone()).activity(
                                    active.as_ref().map(|turn| turn.outbound.id.clone()),
                                    AgentActivity::other(format!("Copilot event warning: {error}")),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(side) = side {
        disconnect_session(&side.slot.session).await.ok();
        if has_side_lease {
            let record = ephemeral_record(
                &config,
                &owner_id,
                &side.operation_id,
                Some(side.parent_id),
                Some(side.id),
                "cleanup_pending",
                now(),
            );
            let (session_id, cleanup_warning) =
                cleanup_ephemeral_record(&client, &ledger, record).await;
            main_events.emit(AgentEvent::OrphanSideCleanup {
                session_id,
                cleanup_warning,
            });
        }
    }
    disconnect_session(&main.session).await.ok();
    sdk_call("Copilot client shutdown", client.stop())
        .await
        .ok();
    Ok(())
}

async fn abort_session(session: &Session) -> Result<()> {
    sdk_call("Copilot abort", session.abort()).await
}

async fn disconnect_session(session: &Session) -> Result<()> {
    sdk_call("Copilot disconnect", session.disconnect()).await
}

async fn delete_session_with_timeout(client: &Client, session_id: &str) -> Result<()> {
    sdk_call(
        "Copilot session deletion",
        client.delete_session(&SessionId::new(session_id)),
    )
    .await
}

async fn sdk_call<T, E, F>(operation: &str, future: F) -> Result<T>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: Into<anyhow::Error>,
{
    tokio::time::timeout(SDK_CONTROL_TIMEOUT, future)
        .await
        .with_context(|| {
            format!(
                "{operation} exceeded {} seconds",
                SDK_CONTROL_TIMEOUT.as_secs()
            )
        })?
        .map_err(Into::into)
}

async fn start_next(
    session: &Session,
    queue: &mut VecDeque<Outbound>,
    active: &mut Option<ActiveOutbound>,
    events: &impl EventOutput,
) {
    let Some(outbound) = queue.pop_front() else {
        return;
    };
    events.activity(
        Some(outbound.id.clone()),
        AgentActivity::other(format!(
            "Dispatching queued request to Copilot SDK · waiting up to {}s for acknowledgement",
            SDK_CONTROL_TIMEOUT.as_secs()
        )),
    );
    // Make the SDK delivery contract explicit. The bridge retains its local
    // queue for UI recovery/state, and every dispatched turn is FIFO at the
    // SDK boundary as well.
    match sdk_call(
        "Copilot message delivery",
        session.send(enqueue_message(&outbound)),
    )
    .await
    {
        Ok(user_message_id) => {
            if env::var_os("RQ_TUI_DEBUG_EVENTS").is_some() {
                eprintln!("rq-tui accepted user-message root={user_message_id}");
            }
            let mut next = ActiveOutbound {
                outbound,
                response_started: false,
                turn_started: false,
                turn_id: None,
                sdk_message_ids: HashSet::new(),
                accepted_event_ids: HashSet::new(),
                message_buffers: HashMap::new(),
                message_order: Vec::new(),
                emitted_text: String::new(),
                hidden_message_ids: HashSet::new(),
            };
            register_sdk_message_root(&mut next, user_message_id);
            *active = Some(next);
        }
        Err(error) => {
            events.emit(AgentEvent::TurnFailed {
                outbound_id: outbound.id.clone(),
                outbound: outbound.kind,
                message: error.to_string(),
                response_started: false,
            });
        }
    }
}

/// Event types that are useful to a terminal client even though they do not
/// carry visible assistant text. Keep this list aligned with the generated
/// SessionEventType names from github-copilot-sdk 1.0.8. The payload remains
/// untyped at this boundary because the CLI can add fields without requiring a
/// bridge release.
fn is_observability_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "session.start"
            | "session.resume"
            | "session.info"
            | "session.warning"
            | "session.remote_steerable_changed"
            | "session.model_change"
            | "session.session_limits_changed"
            | "session.context_changed"
            | "session.usage_info"
            | "session.shutdown"
            | "session.compaction_start"
            | "session.compaction_complete"
            | "assistant.turn_start"
            | "assistant.streaming_delta"
            | "assistant.turn_end"
            | "tool.user_requested"
            | "permission.requested"
            | "permission.completed"
            | "user_input.requested"
            | "user_input.completed"
            | "elicitation.requested"
            | "elicitation.completed"
    )
}

/// Return true for event types which already have a normalizer arm. An event
/// with a new SDK name must not disappear through the wildcard arm below.
fn is_normalized_event(event_type: &str) -> bool {
    is_observability_event(event_type)
        || matches!(
            event_type,
            "user.message"
                | "assistant.intent"
                | "assistant.reasoning_delta"
                | "assistant.turn_retry"
                | "assistant.message_start"
                | "assistant.message_delta"
                | "assistant.message"
                | "assistant.usage"
                | "tool.execution_start"
                | "tool.execution_partial_result"
                | "tool.execution_progress"
                | "tool.execution_complete"
                | "skill.invoked"
                | "subagent.started"
                | "subagent.completed"
                | "subagent.failed"
                | "subagent.selected"
                | "subagent.deselected"
                | "session.task_complete"
                | "session.idle"
                | "session.error"
        )
}

fn sanitize_event_text(text: &str) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !result.is_empty();
            continue;
        }
        if pending_space {
            result.push(' ');
            pending_space = false;
        }
        result.push(character);
        if result.chars().count() >= 160 {
            break;
        }
    }
    if text.chars().count() > result.chars().count() {
        result.push('…');
    }
    result
}

fn event_value_summary(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(sanitize_event_text(value)),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn event_detail(data: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let details = keys
        .iter()
        .filter_map(|key| {
            data.get(*key)
                .and_then(event_value_summary)
                .filter(|value| !value.is_empty())
                .map(|value| format!("{key}={value}"))
        })
        .collect::<Vec<_>>();
    (!details.is_empty()).then(|| details.join(" · "))
}

/// Summarize an event without serializing arbitrary payloads into the UI.
/// In particular, unknown events may contain reasoning, tool arguments, or
/// user content, so only a small allow-list of scalar metadata is displayed.
fn unknown_event_detail(data: &serde_json::Value) -> String {
    const SAFE_KEYS: &[&str] = &[
        "message",
        "reason",
        "status",
        "state",
        "name",
        "toolName",
        "tool_name",
        "requestId",
        "request_id",
        "sessionId",
        "session_id",
        "remoteUrl",
        "remote_url",
        "url",
        "infoType",
        "info_type",
        "mode",
        "success",
        "aborted",
        "turnId",
        "turn_id",
        "model",
        "totalResponseSizeBytes",
        "total_response_size_bytes",
    ];
    if let Some(detail) = event_detail(data, SAFE_KEYS) {
        return detail;
    }
    match data {
        serde_json::Value::Object(values) if values.is_empty() => "payload=empty".into(),
        serde_json::Value::Object(values) => {
            let mut fields = values.keys().cloned().collect::<Vec<_>>();
            fields.sort();
            format!("payload=object fields={}", fields.join(","))
        }
        serde_json::Value::Array(_) => "payload=array".into(),
        serde_json::Value::Null => "payload=null".into(),
        serde_json::Value::String(_) => "payload=string".into(),
        serde_json::Value::Bool(_) => "payload=boolean".into(),
        serde_json::Value::Number(_) => "payload=number".into(),
    }
}

fn emit_unknown_event(
    event: &SessionEvent,
    active: &Option<ActiveOutbound>,
    events: &impl EventOutput,
) {
    events.activity(
        active.as_ref().map(|turn| turn.outbound.id.clone()),
        AgentActivity::other(format!(
            "SDK event {} · {}",
            event.event_type,
            unknown_event_detail(&event.data)
        )),
    );
}

fn handle_session_event(
    event: SessionEvent,
    active: &mut Option<ActiveOutbound>,
    events: &impl EventOutput,
) {
    let root_agent_event = event.agent_id.is_none();
    let observability_event = is_observability_event(&event.event_type);
    let unknown_event = !is_normalized_event(&event.event_type);
    let session_boundary_event = matches!(
        event.event_type.as_str(),
        "session.idle" | "session.error" | "session.task_complete" | "session.shutdown"
    );
    let turn_or_session_event = event.event_type.starts_with("assistant.")
        || event.event_type.starts_with("tool.")
        || event.event_type.starts_with("skill.")
        || event.event_type.starts_with("subagent.")
        || session_boundary_event
        || observability_event
        || unknown_event;
    if let Some(active) = active.as_mut() {
        if event.event_type == "user.message" {
            let matches_sdk_id = active.sdk_message_ids.contains(&event.id)
                || event
                    .data
                    .get("interactionId")
                    .and_then(|value| value.as_str())
                    .is_some_and(|id| active.sdk_message_ids.contains(id));
            let matches_content = event.data.get("content").and_then(|value| value.as_str())
                == Some(active.outbound.text.as_str());
            // Content is only a compatibility fallback for the first prompt.
            // A steering prompt is always registered by the returned SDK ID;
            // accepting it by text after a turn started could bind an unrelated
            // historical user event to this turn.
            if matches_sdk_id || (!active.turn_started && matches_content) {
                active.accepted_event_ids.insert(event.id.clone());
            }
        }
        let chained = active.accepted_event_ids.contains(&event.id)
            || event
                .parent_id
                .as_ref()
                .is_some_and(|parent| active.accepted_event_ids.contains(parent));
        let unparented_observability_event =
            (session_boundary_event || observability_event) && event.parent_id.is_none();
        if turn_or_session_event && !chained && !unparented_observability_event && !unknown_event {
            if env::var_os("RQ_TUI_DEBUG_EVENTS").is_some() {
                eprintln!(
                    "rq-tui rejected SDK event type={} id={} parent={:?} roots={:?}",
                    event.event_type, event.id, event.parent_id, active.accepted_event_ids
                );
            }
            return;
        }
        if chained {
            active.accepted_event_ids.insert(event.id.clone());
        }
        if let (Some(active_turn), Some(event_turn)) = (
            active.turn_id.as_deref(),
            event.data.get("turnId").and_then(|value| value.as_str()),
        ) {
            if active_turn != event_turn {
                return;
            }
        }
    }
    if event.event_type == "assistant.turn_start" && root_agent_event {
        if let Some(active) = active.as_mut() {
            active.turn_started = true;
            active.turn_id = event
                .data
                .get("turnId")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
        }
        send_activity(
            active,
            AgentActivity {
                kind: ActivityKind::Intent,
                label: "Assistant turn started".into(),
                tool: None,
                detail: event
                    .data
                    .get("turnId")
                    .and_then(|value| value.as_str())
                    .map(|turn| format!("SDK turn {turn} started")),
            },
            events,
        );
        return;
    }
    let turn_scoped = event.event_type.starts_with("assistant.")
        || event.event_type.starts_with("tool.")
        || event.event_type.starts_with("skill.")
        || event.event_type.starts_with("subagent.");
    if turn_scoped
        && !observability_event
        && !unknown_event
        && active.as_ref().is_none_or(|active| !active.turn_started)
    {
        // A previous turn can leave buffered deltas behind after its idle
        // boundary. Never attach those bytes to a newly dispatched outbound
        // until that outbound's own turn-start event has arrived.
        return;
    }
    match event.event_type.as_str() {
        "assistant.turn_start" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Intent,
                    label: if root_agent_event {
                        "Assistant turn started".into()
                    } else {
                        "Subagent turn started".into()
                    },
                    tool: Some("assistant_turn".into()),
                    detail: event_detail(&event.data, &["turnId", "model"]),
                },
                events,
            );
        }
        "assistant.turn_end" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolComplete,
                    label: "Assistant turn ended".into(),
                    tool: Some("assistant_turn".into()),
                    detail: event_detail(&event.data, &["turnId", "model"]),
                },
                events,
            );
        }
        "assistant.streaming_delta" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolProgress,
                    label: "Receiving streamed response…".into(),
                    tool: Some("assistant_stream".into()),
                    detail: event_detail(
                        &event.data,
                        &["totalResponseSizeBytes", "total_response_size_bytes"],
                    ),
                },
                events,
            );
        }
        "assistant.intent" if root_agent_event => {
            let intent = event
                .data
                .get("intent")
                .and_then(|value| value.as_str())
                .unwrap_or("Working…");
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Intent,
                    label: intent.into(),
                    tool: None,
                    detail: None,
                },
                events,
            );
        }
        "assistant.reasoning_delta" if root_agent_event => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Reasoning,
                    label: "Reasoning…".into(),
                    tool: None,
                    detail: None,
                },
                events,
            );
        }
        "assistant.turn_retry" if root_agent_event => {
            let reason = event
                .data
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("transient model error");
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Retry,
                    label: format!("Retrying: {reason}"),
                    tool: None,
                    detail: Some(reason.into()),
                },
                events,
            );
        }
        "assistant.message_start" if root_agent_event => {
            if event.data.get("phase").and_then(|value| value.as_str()) == Some("thinking") {
                if let (Some(active), Some(message_id)) = (
                    active.as_mut(),
                    event.data.get("messageId").and_then(|value| value.as_str()),
                ) {
                    active.hidden_message_ids.insert(message_id.to_owned());
                }
            }
        }
        "assistant.message_delta" if root_agent_event => {
            let delta = event
                .data
                .get("deltaContent")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            let message_id = event
                .data
                .get("messageId")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_owned();
            if let Some(active) = active.as_mut() {
                if active.hidden_message_ids.contains(&message_id) {
                    return;
                }
                let new_message = !active.message_buffers.contains_key(&message_id);
                if new_message && active.response_started && !delta.is_empty() {
                    emit_response_piece(active, "\n\n".into(), events);
                }
                if new_message {
                    active.message_order.push(message_id.clone());
                }
                active
                    .message_buffers
                    .entry(message_id)
                    .or_default()
                    .push_str(&delta);
                if !delta.is_empty() {
                    emit_response_piece(active, delta, events);
                }
            }
        }
        "assistant.message" if root_agent_event => {
            if let Some(active) = active.as_mut() {
                let message_id = event
                    .data
                    .get("messageId")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_owned();
                if event.data.get("phase").and_then(|value| value.as_str()) == Some("thinking")
                    || active.hidden_message_ids.contains(&message_id)
                {
                    return;
                }
                let content = event
                    .data
                    .get("content")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned();
                let new_message = !active.message_buffers.contains_key(&message_id);
                if new_message {
                    active.message_order.push(message_id.clone());
                }
                active.message_buffers.insert(message_id, content);
                let snapshot = active
                    .message_order
                    .iter()
                    .filter_map(|id| active.message_buffers.get(id))
                    .filter(|content| !content.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !snapshot.is_empty() && snapshot != active.emitted_text {
                    if active.response_started {
                        active.emitted_text = snapshot.clone();
                        events.emit(AgentEvent::ResponseSnapshot {
                            outbound_id: active.outbound.id.clone(),
                            text: snapshot,
                        });
                    } else {
                        emit_response_piece(active, snapshot, events);
                    }
                }
            }
        }
        "tool.user_requested" => {
            let tool = event
                .data
                .get("toolName")
                .or_else(|| event.data.get("tool_name"))
                .and_then(|value| value.as_str())
                .map(sanitize_event_text)
                .filter(|tool| !tool.is_empty())
                .unwrap_or_else(|| "tool".into());
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolStart,
                    label: format!("User requested {tool}…"),
                    tool: Some(tool),
                    detail: event_detail(&event.data, &["toolCallId", "tool_call_id"]),
                },
                events,
            );
        }
        "tool.execution_start" => {
            let tool = event
                .data
                .get("toolName")
                .and_then(|value| value.as_str())
                .unwrap_or("tool");
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolStart,
                    label: format!("Using {tool}…"),
                    tool: Some(tool.into()),
                    detail: None,
                },
                events,
            );
        }
        "tool.execution_partial_result" => {
            let tool = event
                .data
                .get("toolName")
                .or_else(|| event.data.get("toolCallId"))
                .and_then(|value| value.as_str())
                .unwrap_or("tool");
            let partial = event
                .data
                .get("partialOutput")
                .and_then(|value| value.as_str())
                .unwrap_or("Tool produced partial output");
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolProgress,
                    label: format!("{tool}: partial result"),
                    tool: Some(tool.into()),
                    detail: Some(partial.into()),
                },
                events,
            );
        }
        "tool.execution_progress" => {
            let progress = event
                .data
                .get("progressMessage")
                .and_then(|value| value.as_str())
                .unwrap_or("Tool is working…");
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolProgress,
                    label: progress.into(),
                    tool: event
                        .data
                        .get("toolName")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                    detail: Some(progress.into()),
                },
                events,
            );
        }
        "tool.execution_complete" => {
            let tool = event
                .data
                .get("toolName")
                .or_else(|| event.data.get("tool_name"))
                .and_then(|value| value.as_str())
                .or_else(|| {
                    event
                        .data
                        .get("toolDescription")
                        .or_else(|| event.data.get("tool_description"))
                        .and_then(|description| description.get("name"))
                        .and_then(|value| value.as_str())
                })
                .or_else(|| {
                    event
                        .data
                        .get("toolCallId")
                        .or_else(|| event.data.get("tool_call_id"))
                        .and_then(|value| value.as_str())
                })
                .unwrap_or("tool");
            if let Some(detail) = tool_completion_failure_detail(&event.data) {
                send_activity(
                    active,
                    AgentActivity {
                        kind: ActivityKind::Failure,
                        label: format!("Failed {tool}"),
                        tool: Some(tool.into()),
                        detail: Some(detail),
                    },
                    events,
                );
            } else {
                send_activity(
                    active,
                    AgentActivity {
                        kind: ActivityKind::ToolComplete,
                        label: format!("Finished {tool}"),
                        tool: Some(tool.into()),
                        detail: None,
                    },
                    events,
                );
            }
        }
        "skill.invoked" => {
            let skill = event
                .data
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("skill");
            let detail = event
                .data
                .get("description")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .or_else(|| {
                    event
                        .data
                        .get("source")
                        .and_then(|value| value.as_str())
                        .map(|source| format!("source: {source}"))
                });
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolStart,
                    label: format!("Running skill {skill}…"),
                    tool: Some(format!("skill:{skill}")),
                    detail,
                },
                events,
            );
        }
        "subagent.started" => {
            let name = event
                .data
                .get("agentDisplayName")
                .or_else(|| event.data.get("agentName"))
                .and_then(|value| value.as_str())
                .unwrap_or("subagent");
            let detail = event
                .data
                .get("agentDescription")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .or_else(|| {
                    event
                        .data
                        .get("model")
                        .and_then(|value| value.as_str())
                        .map(|model| format!("model: {model}"))
                });
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolStart,
                    label: format!("Subagent {name} started…"),
                    tool: Some(format!("subagent:{name}")),
                    detail,
                },
                events,
            );
        }
        "subagent.completed" => {
            let name = event
                .data
                .get("agentDisplayName")
                .or_else(|| event.data.get("agentName"))
                .and_then(|value| value.as_str())
                .unwrap_or("subagent");
            let detail = event
                .data
                .get("durationMs")
                .and_then(|value| value.as_i64())
                .map(|duration| format!("completed in {duration}ms"));
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolComplete,
                    label: format!("Subagent {name} completed"),
                    tool: Some(format!("subagent:{name}")),
                    detail,
                },
                events,
            );
        }
        "subagent.failed" => {
            let name = event
                .data
                .get("agentDisplayName")
                .or_else(|| event.data.get("agentName"))
                .and_then(|value| value.as_str())
                .unwrap_or("subagent");
            let detail = event
                .data
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("no error details");
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Failure,
                    label: format!("Subagent {name} failed"),
                    tool: Some(format!("subagent:{name}")),
                    detail: Some(detail.into()),
                },
                events,
            );
        }
        "subagent.selected" => {
            let name = event
                .data
                .get("agentDisplayName")
                .or_else(|| event.data.get("agentName"))
                .and_then(|value| value.as_str())
                .unwrap_or("custom agent");
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Intent,
                    label: format!("Selected subagent {name}"),
                    tool: Some(format!("subagent:{name}")),
                    detail: None,
                },
                events,
            );
        }
        "subagent.deselected" => send_activity(
            active,
            AgentActivity {
                kind: ActivityKind::Intent,
                label: "Returned to the default agent".into(),
                tool: None,
                detail: None,
            },
            events,
        ),
        "session.compaction_start" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolProgress,
                    label: "Compacting conversation context…".into(),
                    tool: Some("context_compaction".into()),
                    detail: event_detail(
                        &event.data,
                        &[
                            "model",
                            "conversationTokens",
                            "systemTokens",
                            "toolDefinitionsTokens",
                        ],
                    ),
                },
                events,
            );
        }
        "session.compaction_complete" => {
            let succeeded = event.data.get("success").and_then(|value| value.as_bool());
            if succeeded == Some(false) {
                send_activity(
                    active,
                    AgentActivity {
                        kind: ActivityKind::Failure,
                        label: "Context compaction failed".into(),
                        tool: Some("context_compaction".into()),
                        detail: event_detail(&event.data, &["error", "statusCode"]),
                    },
                    events,
                );
            } else {
                send_activity(
                    active,
                    AgentActivity {
                        kind: ActivityKind::ToolComplete,
                        label: "Conversation context compacted".into(),
                        tool: Some("context_compaction".into()),
                        detail: event_detail(
                            &event.data,
                            &["tokensRemoved", "postCompactionTokens", "success"],
                        ),
                    },
                    events,
                );
                if succeeded == Some(true) {
                    events.emit(AgentEvent::Compacted);
                }
            }
        }
        "permission.requested" => {
            let permission_kind = event
                .data
                .get("permissionRequest")
                .or_else(|| event.data.get("permission_request"))
                .and_then(|value| value.get("kind"))
                .and_then(event_value_summary);
            let request_id = event_detail(&event.data, &["requestId", "request_id"]);
            let detail = permission_kind
                .map(|kind| format!("kind={kind}"))
                .into_iter()
                .chain(request_id)
                .collect::<Vec<_>>();
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: "Permission requested · waiting for policy decision".into(),
                    tool: Some("permission".into()),
                    detail: (!detail.is_empty()).then(|| detail.join(" · ")),
                },
                events,
            );
        }
        "permission.completed" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolComplete,
                    label: "Permission request resolved".into(),
                    tool: Some("permission".into()),
                    detail: event_detail(&event.data, &["requestId", "request_id"]),
                },
                events,
            );
        }
        "user_input.requested" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: "Copilot is waiting for your input".into(),
                    tool: Some("user_input".into()),
                    detail: event_detail(&event.data, &["question", "requestId", "request_id"]),
                },
                events,
            );
        }
        "user_input.completed" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolComplete,
                    label: "User input received".into(),
                    tool: Some("user_input".into()),
                    detail: event_detail(&event.data, &["requestId", "request_id"]),
                },
                events,
            );
        }
        "elicitation.requested" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: "Copilot is waiting for confirmation".into(),
                    tool: Some("elicitation".into()),
                    detail: event_detail(
                        &event.data,
                        &["message", "mode", "requestId", "request_id"],
                    ),
                },
                events,
            );
        }
        "elicitation.completed" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolComplete,
                    label: "User confirmation received".into(),
                    tool: Some("elicitation".into()),
                    detail: event_detail(&event.data, &["action", "requestId", "request_id"]),
                },
                events,
            );
        }
        "session.info" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: "Copilot session info".into(),
                    tool: Some("session_info".into()),
                    detail: event_detail(&event.data, &["message", "infoType", "url"]),
                },
                events,
            );
        }
        "session.warning" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Failure,
                    label: "Copilot session warning".into(),
                    tool: Some("session_warning".into()),
                    detail: event_detail(&event.data, &["message", "warningType", "url"]),
                },
                events,
            );
        }
        "session.remote_steerable_changed" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: "Remote session steering state changed".into(),
                    tool: Some("remote_session".into()),
                    detail: event_detail(
                        &event.data,
                        &["remoteSteerable", "steerable", "status", "url"],
                    ),
                },
                events,
            );
        }
        "session.start" | "session.resume" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: if event.event_type == "session.resume" {
                        "Copilot session resumed".into()
                    } else {
                        "Copilot session started".into()
                    },
                    tool: Some("session".into()),
                    detail: event_detail(&event.data, &["sessionId", "session_id", "url"]),
                },
                events,
            );
        }
        "session.model_change"
        | "session.session_limits_changed"
        | "session.context_changed"
        | "session.usage_info" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: format!("Copilot {}", event.event_type.replace("session.", "")),
                    tool: Some("session_metadata".into()),
                    detail: Some(unknown_event_detail(&event.data)),
                },
                events,
            );
        }
        "session.shutdown" => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Other,
                    label: "Copilot session shutting down".into(),
                    tool: Some("session".into()),
                    detail: event_detail(&event.data, &["reason", "message"]),
                },
                events,
            );
        }
        "assistant.usage" if root_agent_event => {
            events.emit(AgentEvent::Usage {
                model: event
                    .data
                    .get("model")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                input_tokens: event
                    .data
                    .get("inputTokens")
                    .and_then(|value| value.as_i64()),
                output_tokens: event
                    .data
                    .get("outputTokens")
                    .and_then(|value| value.as_i64()),
                cache_read_tokens: event
                    .data
                    .get("cacheReadTokens")
                    .and_then(|value| value.as_i64()),
            });
        }
        "session.task_complete" => {
            let summary = event
                .data
                .get("summary")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::ToolComplete,
                    label: "Copilot marked the task complete".into(),
                    tool: Some("task_complete".into()),
                    detail: summary,
                },
                events,
            );
        }
        "session.idle" => {
            if active.as_ref().is_some_and(|active| !active.turn_started) {
                events.activity(
                    active.as_ref().map(|turn| turn.outbound.id.clone()),
                    AgentActivity::other(
                        "Ignored an idle boundary from the previous turn; waiting for this turn to start",
                    ),
                );
            } else if background_tasks_are_running(event.data.get("backgroundTasks")) {
                send_activity(
                    active,
                    AgentActivity {
                        kind: ActivityKind::ToolProgress,
                        label: "Copilot is still running background work".into(),
                        tool: Some("background_tasks".into()),
                        detail: Some(
                            "Waiting for Copilot to report that its background tasks are done"
                                .into(),
                        ),
                    },
                    events,
                );
            } else if let Some(active) = active.take() {
                let aborted = event
                    .data
                    .get("aborted")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if !active.response_started {
                    events.emit(AgentEvent::ResponseStarted {
                        outbound_id: active.outbound.id.clone(),
                        outbound: active.outbound.kind.clone(),
                        first_delta: String::new(),
                    });
                }
                events.emit(AgentEvent::ResponseComplete {
                    outbound_id: active.outbound.id,
                    aborted,
                });
            }
        }
        "session.error" => {
            let message = event
                .data
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("Copilot session error")
                .to_owned();
            if active.is_some() {
                fail_active(active, message, events);
            } else {
                events.activity(
                    None,
                    AgentActivity::other(format!("Copilot error: {message}")),
                );
            }
        }
        _ if unknown_event => emit_unknown_event(&event, active, events),
        _ => {}
    }
}

fn register_sdk_message_root(active: &mut ActiveOutbound, message_id: String) {
    if message_id.is_empty() {
        return;
    }
    active.sdk_message_ids.insert(message_id.clone());
    // SDK-assigned IDs are trusted roots. Some CLI versions use this exact ID
    // as a parent while others expose it via `user.message.interactionId`.
    active.accepted_event_ids.insert(message_id);
}

/// Return a user-facing diagnostic for a failed tool completion.
///
/// The pinned SDK models the normal completion wire shape with `success` and
/// `error`, but raw events can also carry MCP's `result.isError` marker. Keep
/// both camelCase and snake_case spellings here because this boundary consumes
/// the CLI's untyped JSON event envelope rather than the generated Rust DTO.
fn tool_completion_failure_detail(data: &serde_json::Value) -> Option<String> {
    let mut details = Vec::new();

    if let Some(error) = data.get("error") {
        if let Some(message) = error.as_str() {
            if !message.is_empty() {
                details.push(message.to_owned());
            }
        } else if let Some(message) = error.get("message").and_then(|value| value.as_str()) {
            if !message.is_empty() {
                if let Some(code) = error.get("code").and_then(|value| value.as_str()) {
                    details.push(format!("{code}: {message}"));
                } else {
                    details.push(message.to_owned());
                }
            }
        }
    }

    let result = data.get("result");
    let result_is_error = result
        .and_then(|result| result.get("isError").or_else(|| result.get("is_error")))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if result_is_error {
        if let Some(message) = result
            .and_then(|result| {
                result
                    .get("detailedContent")
                    .or_else(|| result.get("detailed_content"))
            })
            .and_then(|value| value.as_str())
            .filter(|message| !message.is_empty())
        {
            details.push(message.to_owned());
        } else if let Some(message) = result
            .and_then(|result| result.get("content"))
            .and_then(|value| value.as_str())
            .filter(|message| !message.is_empty())
        {
            details.push(message.to_owned());
        } else {
            details.push("tool result marked as an error".into());
        }
    }

    if data.get("success").and_then(|value| value.as_bool()) == Some(false) && details.is_empty() {
        details.push("tool execution reported success=false".into());
    }

    (!details.is_empty()).then(|| details.join("; "))
}

fn background_tasks_are_running(background_tasks: Option<&serde_json::Value>) -> bool {
    match background_tasks {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(running)) => *running,
        Some(serde_json::Value::Number(count)) => count.as_u64().is_none_or(|count| count > 0),
        Some(serde_json::Value::String(status)) => !status.is_empty(),
        Some(serde_json::Value::Array(tasks)) => !tasks.is_empty(),
        Some(serde_json::Value::Object(tasks)) => tasks
            .values()
            .any(|task| background_tasks_are_running(Some(task))),
    }
}

fn emit_response_piece(active: &mut ActiveOutbound, piece: String, events: &impl EventOutput) {
    active.emitted_text.push_str(&piece);
    if active.response_started {
        events.emit(AgentEvent::ResponseDelta {
            outbound_id: active.outbound.id.clone(),
            delta: piece,
        });
    } else {
        active.response_started = true;
        events.emit(AgentEvent::ResponseStarted {
            outbound_id: active.outbound.id.clone(),
            outbound: active.outbound.kind.clone(),
            first_delta: piece,
        });
    }
}

fn send_activity(
    active: &Option<ActiveOutbound>,
    activity: AgentActivity,
    events: &impl EventOutput,
) {
    events.activity(
        active.as_ref().map(|turn| turn.outbound.id.clone()),
        activity,
    );
}

fn fail_active(active: &mut Option<ActiveOutbound>, message: String, events: &impl EventOutput) {
    if let Some(active) = active.take() {
        events.emit(AgentEvent::TurnFailed {
            outbound_id: active.outbound.id,
            outbound: active.outbound.kind,
            message,
            response_started: active.response_started,
        });
    }
}

fn fail_queue(queue: &mut VecDeque<Outbound>, message: &str, events: &impl EventOutput) {
    while let Some(outbound) = queue.pop_front() {
        events.emit(AgentEvent::TurnFailed {
            outbound_id: outbound.id,
            outbound: outbound.kind,
            message: message.to_owned(),
            response_started: false,
        });
    }
}

fn resumed_active_from_history(
    events: &[SessionEvent],
    session_id: &SessionId,
) -> Option<ActiveOutbound> {
    let mut last_user: Option<(usize, &SessionEvent)> = None;
    let mut last_completed_index = None;
    let mut open_turn: Option<(String, &SessionEvent)> = None;

    for (index, event) in events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.agent_id.is_none())
    {
        match event.event_type.as_str() {
            "user.message" => last_user = Some((index, event)),
            "assistant.turn_start" => {
                open_turn = Some((
                    event
                        .data
                        .get("turnId")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                        .to_owned(),
                    event,
                ));
            }
            "assistant.turn_end" => {
                let ended = event.data.get("turnId").and_then(|value| value.as_str());
                if open_turn
                    .as_ref()
                    .is_some_and(|(turn_id, _)| ended == Some(turn_id.as_str()))
                {
                    open_turn = None;
                }
                last_completed_index = Some(index);
            }
            "session.idle" | "session.error" | "session.shutdown" => {
                open_turn = None;
                last_completed_index = Some(index);
            }
            _ => {}
        }
    }

    let pending_event = open_turn.as_ref().map(|(_, event)| *event).or_else(|| {
        last_user
            .filter(|(index, _)| last_completed_index.is_none_or(|done| *index > done))
            .map(|(_, event)| event)
    })?;
    let pending_index = events
        .iter()
        .position(|event| event.id == pending_event.id)
        .unwrap_or(0);
    let turn_id = open_turn.as_ref().map(|(turn_id, _)| turn_id.clone());
    Some(ActiveOutbound {
        outbound: Outbound {
            id: format!("resumed:{}:{}", session_id, pending_event.id),
            kind: OutboundKind::Chat,
            text: String::new(),
        },
        response_started: false,
        turn_started: turn_id.is_some(),
        turn_id,
        sdk_message_ids: HashSet::new(),
        accepted_event_ids: events[pending_index..]
            .iter()
            .map(|event| event.id.clone())
            .collect(),
        message_buffers: HashMap::new(),
        message_order: Vec::new(),
        emitted_text: String::new(),
        hidden_message_ids: HashSet::new(),
    })
}

fn history_entries(events: &[SessionEvent]) -> Vec<HistoryEntry> {
    events
        .iter()
        .filter(|event| event.agent_id.is_none())
        .filter_map(|event| {
            let (role, text) = match event.event_type.as_str() {
                "user.message" => (
                    "you",
                    sanitize_history_user(
                        event
                            .data
                            .get("content")
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                    ),
                ),
                "assistant.message"
                    if event.data.get("phase").and_then(|value| value.as_str())
                        != Some("thinking") =>
                {
                    (
                        "copilot",
                        event
                            .data
                            .get("content")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                            .to_owned(),
                    )
                }
                _ => return None,
            };
            (!text.trim().is_empty()).then_some(HistoryEntry {
                role: role.to_owned(),
                text,
            })
        })
        .collect()
}

fn sanitize_history_user(content: &str) -> String {
    if let Some((_, question)) = content.split_once("\nQuestion: ") {
        return question.to_owned();
    }
    if content.starts_with("Code review comments submitted for ") {
        return content
            .lines()
            .next()
            .unwrap_or("Code review comments submitted")
            .to_owned();
    }
    if content.starts_with("Draft structured review context") {
        return "Generate structured review context".into();
    }
    content.to_owned()
}

struct SessionBoot {
    session: Session,
    resumed: bool,
    resume_warning: Option<String>,
}

async fn create_or_resume_session(
    client: &Client,
    config: &BridgeConfig,
    events: &EventPublisher,
    ledger: &Storage,
) -> Result<SessionBoot> {
    if let Some(id) = &config.existing_session_id {
        match client
            .resume_session(resume_config(
                SessionId::new(id.clone()),
                config,
                Some(events),
            ))
            .await
        {
            Ok(session) => {
                return Ok(SessionBoot {
                    session,
                    resumed: true,
                    resume_warning: None,
                });
            }
            Err(error) => {
                let session = create_tracked_session(client, config, events, ledger).await?;
                return Ok(SessionBoot {
                    session,
                    resumed: false,
                    resume_warning: Some(format!(
                        "Could not resume session {id}; started a replacement: {error}"
                    )),
                });
            }
        }
    }
    Ok(SessionBoot {
        session: create_tracked_session(client, config, events, ledger).await?,
        resumed: false,
        resume_warning: None,
    })
}

async fn create_tracked_session(
    client: &Client,
    config: &BridgeConfig,
    events: &EventPublisher,
    ledger: &Storage,
) -> Result<Session> {
    let session_id = Uuid::new_v4().to_string();
    // Persist the client-selected ID before asking the SDK to create it. A
    // crash at any later instruction leaves an idempotent prune target rather
    // than an untracked remote session.
    ledger.activate_session(&SessionRecord {
        id: session_id.clone(),
        work_item_id: config.work_item_id.clone(),
        parent_id: None,
        active: true,
        created_at: now(),
    })?;
    let session = client
        .create_session(
            create_config(config, Some(events)).with_session_id(SessionId::new(session_id.clone())),
        )
        .await?;
    if session.id().as_str() != session_id {
        let unexpected_id = session.id().to_string();
        disconnect_session(&session).await.ok();
        let cleanup = client
            .delete_session_if_present(&unexpected_id)
            .await
            .err()
            .map(|error| format!("; cleanup failed: {error}"))
            .unwrap_or_default();
        anyhow::bail!(
            "Copilot created session {unexpected_id} instead of the pre-journaled id {session_id}{cleanup}"
        );
    }
    Ok(session)
}

fn create_config(config: &BridgeConfig, events: Option<&EventPublisher>) -> SessionConfig {
    let mut session = SessionConfig::default()
        .with_model(config.model.clone())
        .with_streaming(true)
        .with_working_directory(config.session_root.clone())
        .with_skill_directories(config.skill_directories.clone())
        .with_plugin_directories(config.plugin_directories.clone())
        .with_system_message(system_message(&config.work_item_id))
        .with_permission_handler(Arc::new(ReadOnlyPermissionHandler));
    if let Some(effort) = &config.reasoning_effort {
        session = session.with_reasoning_effort(effort.clone());
    }
    if let Some(tier) = &config.context_tier {
        session = session.with_context_tier(tier.clone());
    }
    if let Some(events) = events {
        session = session.with_hooks(progress_hooks(events));
    }
    session
}

fn resume_config(
    id: SessionId,
    config: &BridgeConfig,
    events: Option<&EventPublisher>,
) -> ResumeSessionConfig {
    let mut session = ResumeSessionConfig::new(id)
        // A bridge restart must hand pending SDK work back to the resumed
        // session rather than silently abandoning tool/permission work.
        .with_continue_pending_work(true)
        .with_model(config.model.clone())
        .with_streaming(true)
        .with_working_directory(config.session_root.clone())
        .with_skill_directories(config.skill_directories.clone())
        .with_plugin_directories(config.plugin_directories.clone())
        .with_system_message(system_message(&config.work_item_id))
        .with_permission_handler(Arc::new(ReadOnlyPermissionHandler));
    if let Some(effort) = &config.reasoning_effort {
        session = session.with_reasoning_effort(effort.clone());
    }
    if let Some(tier) = &config.context_tier {
        session = session.with_context_tier(tier.clone());
    }
    if let Some(events) = events {
        session = session.with_hooks(progress_hooks(events));
    }
    session
}

fn system_message(work_item_id: &str) -> SystemMessageConfig {
    SystemMessageConfig::new().with_content(format!(
        "You are the conversational agent inside rq-tui, a code-review harness. \
         The persistent Work Item id is {work_item_id}. \
         Ground answers in the checked-out repositories. For inline asks, answer \
         the supplied question and anchor directly. Review metadata and accepted \
         structured context are under .rq-tui when present. Operate read-only: inspect and \
         search files, but never modify files, execute shell commands, write memory, \
         or contact external tools."
    ))
}

#[derive(Debug)]
struct ReadOnlyPermissionHandler;

#[async_trait]
impl PermissionHandler for ReadOnlyPermissionHandler {
    async fn handle(
        &self,
        _session_id: SessionId,
        _request_id: RequestId,
        data: PermissionRequestData,
    ) -> PermissionResult {
        match data.kind {
            Some(PermissionRequestKind::Read) => PermissionResult::approve_once(),
            Some(PermissionRequestKind::CustomTool) => {
                let tool = data
                    .extra
                    .get("tool")
                    .or_else(|| data.extra.get("toolName"))
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if [
                    "read",
                    "view",
                    "search",
                    "grep",
                    "glob",
                    "read_file",
                    "view_file",
                    "search_files",
                    "grep_search",
                    "file_search",
                ]
                .contains(&tool.as_str())
                {
                    PermissionResult::approve_once()
                } else {
                    PermissionResult::reject(Some("rq-tui asks are read-only".into()))
                }
            }
            _ => PermissionResult::reject(Some("rq-tui asks are read-only".into())),
        }
    }
}

fn find_copilot_cli() -> Option<PathBuf> {
    if let Some(path) = env::var_os("COPILOT_CLI_PATH").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let executable = if cfg!(windows) {
        "copilot.exe"
    } else {
        "copilot"
    };
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(executable))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::env;
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use github_copilot_sdk::handler::PermissionHandler;
    use github_copilot_sdk::hooks::{
        HookContext, HookEvent, PostToolUseFailureInput, PreToolUseInput, SessionHooks,
    };
    use github_copilot_sdk::{
        PermissionRequestData, PermissionRequestKind, RequestId, SessionEvent, SessionId,
    };

    use super::{
        cleanup_ephemeral_record, create_config, delete_journaled_remote_sessions,
        durable_export_choice, enqueue_message, envelope_outbound_id, execute_prune_operation,
        handle_session_event, history_entries, model_option, now, register_sdk_message_root,
        resume_config, resumed_active_from_history, retryable_cleanup_state, ActiveOutbound,
        ActivityKind, AgentCommand, AgentEvent, AgentLane, AgentRuntime, AgentSink, BridgeConfig,
        ControlledAgent, CopilotBridge, EventPublisher, HistoryEntry, LaneEvent, Outbound,
        OutboundKind, ProgressHooks, PruneExecution, ReadOnlyPermissionHandler, SideCleanupBackend,
        WorkItemProcessLock,
    };
    use crate::config::AppPaths;
    use crate::domain::{EphemeralSessionRecord, SessionRecord, WorkItem};
    use crate::storage::{PruneOperation, Storage};

    fn test_app_paths(root: PathBuf) -> AppPaths {
        AppPaths {
            data: root.join("data"),
            cache: root.join("cache"),
            database: root.join("data").join("review.db"),
            roots: root.join("data").join("roots"),
            prs: root.join("cache").join("prs"),
            exports: root.join("data").join("exports"),
            skills: root.join("data").join("skills"),
            plugins: root.join("data").join("plugins"),
        }
    }

    #[derive(Default)]
    struct FakeSideCleanup {
        named_sessions: Mutex<HashMap<String, String>>,
        deleted_sessions: Mutex<Vec<String>>,
        delete_error: Mutex<Option<String>>,
        absent_sessions: Mutex<HashSet<String>>,
        successful_delete_sticks: Mutex<bool>,
        failed_delete_removes: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl super::SideCleanupBackend for FakeSideCleanup {
        async fn reconcile_side_id(
            &self,
            operation_id: &str,
            parent_id: &str,
        ) -> Result<Option<String>> {
            Ok(self
                .named_sessions
                .lock()
                .unwrap()
                .get(&super::side_session_name(operation_id, parent_id))
                .cloned())
        }

        async fn session_exists(&self, session_id: &str) -> Result<bool> {
            Ok(!self.absent_sessions.lock().unwrap().contains(session_id))
        }

        async fn delete_session(&self, session_id: &str) -> Result<()> {
            if let Some(error) = self.delete_error.lock().unwrap().clone() {
                if *self.failed_delete_removes.lock().unwrap() {
                    self.absent_sessions
                        .lock()
                        .unwrap()
                        .insert(session_id.to_owned());
                }
                anyhow::bail!(error);
            }
            self.deleted_sessions
                .lock()
                .unwrap()
                .push(session_id.to_owned());
            if !*self.successful_delete_sticks.lock().unwrap() {
                self.absent_sessions
                    .lock()
                    .unwrap()
                    .insert(session_id.to_owned());
            }
            Ok(())
        }
    }

    fn cleanup_storage() -> (Storage, WorkItem) {
        let storage = Storage::in_memory().unwrap();
        let item = WorkItem {
            id: "cleanup-work".into(),
            name: "cleanup".into(),
            workspace_root: PathBuf::from("/cleanup"),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_opened_at: None,
        };
        storage.upsert_work_item(&item).unwrap();
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner-1", 1_000, 0)
            .unwrap());
        (storage, item)
    }

    fn cleanup_record(item: &WorkItem, side_id: Option<&str>) -> EphemeralSessionRecord {
        EphemeralSessionRecord {
            operation_id: "operation-1".into(),
            work_item_id: item.id.clone(),
            owner_id: "owner-1".into(),
            parent_id: Some("main-1".into()),
            side_id: side_id.map(str::to_owned),
            state: "cleanup_pending".into(),
            last_error: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[test]
    fn orphan_cleanup_reconciles_named_fork_and_clears_ledger() {
        let (storage, item) = cleanup_storage();
        let record = cleanup_record(&item, None);
        assert!(storage.record_ephemeral_session(&record).unwrap());
        let backend = FakeSideCleanup::default();
        backend.named_sessions.lock().unwrap().insert(
            super::side_session_name(&record.operation_id, record.parent_id.as_deref().unwrap()),
            "side-1".into(),
        );

        let (session_id, warning) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(cleanup_ephemeral_record(&backend, &storage, record));

        assert_eq!(session_id, "side-1");
        assert!(warning.is_none());
        assert_eq!(
            backend.deleted_sessions.lock().unwrap().as_slice(),
            ["side-1"]
        );
        assert!(storage.ephemeral_sessions(&item.id).unwrap().is_empty());
    }

    #[test]
    fn side_reconciliation_rejects_a_name_bound_to_another_parent() {
        let (storage, item) = cleanup_storage();
        let record = cleanup_record(&item, None);
        assert!(storage.record_ephemeral_session(&record).unwrap());
        let backend = FakeSideCleanup::default();
        backend.named_sessions.lock().unwrap().insert(
            super::side_session_name(&record.operation_id, "different-parent"),
            "unrelated-side".into(),
        );

        let (_, warning) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(cleanup_ephemeral_record(&backend, &storage, record));

        assert!(warning
            .as_deref()
            .is_some_and(|message| message.contains("still unresolved")));
        assert!(backend.deleted_sessions.lock().unwrap().is_empty());
        assert_eq!(storage.ephemeral_sessions(&item.id).unwrap().len(), 1);
    }

    #[test]
    fn unresolved_fork_intent_is_never_discarded_after_one_empty_reconciliation() {
        let (storage, item) = cleanup_storage();
        let record = cleanup_record(&item, None);
        assert!(storage.record_ephemeral_session(&record).unwrap());
        let backend = FakeSideCleanup::default();

        let (_, warning) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(cleanup_ephemeral_record(&backend, &storage, record));

        assert!(warning
            .as_deref()
            .is_some_and(|message| message.contains("still unresolved")));
        let retained = storage.ephemeral_sessions(&item.id).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].state, "cleanup_pending");
        assert!(backend.deleted_sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_side_deletion_remains_durable_for_next_startup() {
        let (storage, item) = cleanup_storage();
        let record = cleanup_record(&item, Some("side-1"));
        assert!(storage.record_ephemeral_session(&record).unwrap());
        let backend = FakeSideCleanup::default();
        *backend.delete_error.lock().unwrap() = Some("controlled delete failure".into());

        let (_, warning) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(cleanup_ephemeral_record(&backend, &storage, record));

        assert!(warning
            .as_deref()
            .is_some_and(|message| message.contains("controlled delete failure")));
        let retained = storage.ephemeral_sessions(&item.id).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].state, "cleanup_pending");
        assert!(retained[0]
            .last_error
            .as_deref()
            .is_some_and(|message| message.contains("controlled delete failure")));
    }

    #[test]
    fn stale_cleanup_owner_never_calls_external_delete() {
        let (storage, item) = cleanup_storage();
        let mut record = cleanup_record(&item, Some("side-1"));
        record.state = "deleting".into();
        assert!(storage.record_ephemeral_session(&record).unwrap());
        assert!(storage
            .claim_ephemeral_session_lease(&item.id, "owner-2", 2_000, 1_500)
            .unwrap());
        let backend = FakeSideCleanup::default();

        let (_, warning) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(cleanup_ephemeral_record(&backend, &storage, record));

        assert!(warning
            .as_deref()
            .is_some_and(|message| message.contains("ownership changed")));
        assert!(backend.deleted_sessions.lock().unwrap().is_empty());
        assert_eq!(
            storage.ephemeral_sessions(&item.id).unwrap()[0].owner_id,
            "owner-2"
        );
    }

    #[test]
    fn prune_deletes_inactive_main_forks_and_reports_progress() {
        let (storage, item) = cleanup_storage();
        storage
            .activate_session(&SessionRecord {
                id: "main-parent".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "main-fork".into(),
                work_item_id: item.id.clone(),
                parent_id: Some("main-parent".into()),
                active: true,
                created_at: "2".into(),
            })
            .unwrap();
        let backend = FakeSideCleanup::default();
        let mut progress = Vec::new();
        let operation_id = storage
            .begin_prune_operation("prune-operation", &item.id, false)
            .unwrap();

        let outcome =
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(delete_journaled_remote_sessions(
                    &backend,
                    &storage,
                    "owner-1",
                    &operation_id,
                    &item.id,
                    |label, completed, total| progress.push((label, completed, total)),
                ));

        assert!(outcome.is_ok());
        assert_eq!(
            backend.deleted_sessions.lock().unwrap().as_slice(),
            ["main-fork", "main-parent"]
        );
        assert!(progress
            .iter()
            .any(|(_, completed, total)| *completed == 2 && *total == 2));
    }

    #[test]
    fn session_deletion_is_absence_checked_before_and_after_the_sdk_call() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let already_absent = FakeSideCleanup::default();
        already_absent
            .absent_sessions
            .lock()
            .unwrap()
            .insert("missing".into());
        runtime
            .block_on(already_absent.delete_session_if_present("missing"))
            .unwrap();
        assert!(already_absent.deleted_sessions.lock().unwrap().is_empty());

        let sticky = FakeSideCleanup::default();
        *sticky.successful_delete_sticks.lock().unwrap() = true;
        let error = runtime
            .block_on(sticky.delete_session_if_present("still-present"))
            .unwrap_err();
        assert!(error.to_string().contains("still exists"));

        let error_but_absent = FakeSideCleanup::default();
        *error_but_absent.delete_error.lock().unwrap() = Some("transport failed".into());
        *error_but_absent.failed_delete_removes.lock().unwrap() = true;
        runtime
            .block_on(error_but_absent.delete_session_if_present("deleted-remotely"))
            .unwrap();
    }

    #[test]
    fn durable_prune_retries_remote_failure_then_finishes_local_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_app_paths(directory.path().to_path_buf());
        paths.ensure().unwrap();
        let storage = Storage::open(&paths.database).unwrap();
        let item = WorkItem {
            id: "durable-prune".into(),
            name: "durable prune".into(),
            workspace_root: directory.path().join("workspace"),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_opened_at: Some("1".into()),
        };
        storage.upsert_work_item(&item).unwrap();
        storage
            .activate_session(&SessionRecord {
                id: "persistent-session".into(),
                work_item_id: item.id.clone(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        let backend = FakeSideCleanup::default();
        *backend.delete_error.lock().unwrap() = Some("controlled remote failure".into());

        let first = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(execute_prune_operation(
                &backend,
                PruneExecution {
                    ledger: &storage,
                    database_path: &paths.database,
                    owner_id: "prune-owner",
                    requested_operation_id: "first-request",
                    work_item_id: &item.id,
                    export_first: false,
                    paths: &paths,
                },
                |_, _, _| {},
            ));

        assert!(
            first
                .error
                .as_deref()
                .is_some_and(|error| error.contains("controlled remote failure")),
            "{:?}",
            first.error
        );
        assert!(storage.work_item_by_id(&item.id).unwrap().is_some());
        let journal = storage.pending_prune_operations().unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].phase, "failed");
        *backend.delete_error.lock().unwrap() = None;

        let second = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(execute_prune_operation(
                &backend,
                PruneExecution {
                    ledger: &storage,
                    database_path: &paths.database,
                    owner_id: "prune-owner",
                    requested_operation_id: "second-request",
                    work_item_id: &item.id,
                    export_first: false,
                    paths: &paths,
                },
                |_, _, _| {},
            ));

        assert!(second.error.is_none(), "{:?}", second.error);
        assert!(storage.work_item_by_id(&item.id).unwrap().is_none());
        assert!(storage.pending_prune_operations().unwrap().is_empty());
        assert_eq!(
            backend.deleted_sessions.lock().unwrap().as_slice(),
            ["persistent-session"]
        );
    }

    #[test]
    fn interrupted_deleting_state_is_retryable_without_restart() {
        assert!(retryable_cleanup_state("cleanup_pending"));
        assert!(retryable_cleanup_state("deleting"));
        assert!(!retryable_cleanup_state("active"));
        assert!(!retryable_cleanup_state("opening"));
    }

    #[test]
    fn prune_retry_keeps_the_original_durable_export_choice() {
        let operation = PruneOperation {
            operation_id: "operation".into(),
            work_item_id: "work".into(),
            export_first: true,
            phase: "failed".into(),
            last_error: Some("retry".into()),
            created_at: "1".into(),
            updated_at: "2".into(),
        };

        assert!(durable_export_choice(Some(&operation), false));
        assert!(!durable_export_choice(None, false));
    }

    #[test]
    fn outbound_queue_types_are_sendable() {
        fn assert_send<T: Send>() {}
        assert_send::<super::AgentCommand>();
        assert_send::<super::AgentEvent>();
    }

    #[test]
    fn model_mapping_preserves_runtime_picker_capabilities() {
        let model = github_copilot_sdk::Model {
            id: "runtime-model".into(),
            name: "Runtime Model".into(),
            supported_reasoning_efforts: Some(vec!["low".into(), "high".into()]),
            default_reasoning_effort: Some("high".into()),
            ..Default::default()
        };
        let option = model_option(&model);
        assert_eq!(option.id, "runtime-model");
        assert_eq!(option.name, "Runtime Model");
        assert_eq!(option.supported_reasoning_efforts, ["low", "high"]);
        assert_eq!(option.default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(option.context_tiers[0].id, "default");
    }

    #[test]
    fn controlled_model_listing_is_deterministic_and_staged() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned(); // SessionReady.
        agent.send(AgentCommand::ListModels).expect("list models");
        let event = agent.try_recv_laned().expect("models listed");
        assert!(matches!(
            event.event,
            LaneEvent::Agent(AgentEvent::ModelsListed(ref models))
                if models.len() == 2
                    && models[0].id == "controlled-fast"
                    && models[1].context_tiers.iter().any(|tier| tier.id == "long_context")
        ));
    }

    #[test]
    fn worker_defense_in_depth_rejects_pruning_its_open_work_item() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_app_paths(directory.path().to_path_buf());
        paths.ensure().unwrap();
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned(); // SessionReady.

        agent
            .send(AgentCommand::PruneSessions {
                request_id: "prune-current".into(),
                work_item_ids: vec!["work-item".into()],
                export_first: false,
                paths,
            })
            .expect("worker returns a typed per-item rejection");

        let started = agent.try_recv_laned().expect("prune started");
        assert!(matches!(
            started.event,
            LaneEvent::Agent(AgentEvent::PruneSessionsStarted {
                ref request_id,
                work_items: 1,
            }) if request_id == "prune-current"
        ));
        let deadline = Instant::now() + Duration::from_secs(1);
        let complete = loop {
            if let Some(event) = agent.try_recv_laned() {
                break event;
            }
            assert!(Instant::now() < deadline, "prune complete");
            std::thread::yield_now();
        };
        assert!(matches!(
            complete.event,
            LaneEvent::Agent(AgentEvent::PruneSessionsComplete {
                ref request_id,
                ref outcomes,
            }) if request_id == "prune-current"
                && outcomes.len() == 1
                && outcomes[0].work_item_id == "work-item"
                && outcomes[0]
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("open Work Item"))
        ));
    }

    #[test]
    fn process_locks_are_exclusive_per_work_item_but_independent_across_items() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_app_paths(directory.path().to_path_buf());
        paths.ensure().unwrap();

        let first = WorkItemProcessLock::try_acquire(&paths, "work-a")
            .unwrap()
            .expect("first lock");
        assert!(WorkItemProcessLock::try_acquire(&paths, "work-a")
            .unwrap()
            .is_none());
        assert!(WorkItemProcessLock::try_acquire(&paths, "work-b")
            .unwrap()
            .is_some());

        drop(first);
        assert!(WorkItemProcessLock::try_acquire(&paths, "work-a")
            .unwrap()
            .is_some());
    }

    #[test]
    fn controlled_runtime_replays_startup_prune_recovery_through_typed_events() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_app_paths(directory.path().to_path_buf());
        paths.ensure().unwrap();
        let storage = Storage::open(&paths.database).unwrap();
        for id in ["current", "interrupted"] {
            storage
                .upsert_work_item(&WorkItem {
                    id: id.into(),
                    name: id.into(),
                    workspace_root: directory.path().join(id),
                    created_at: "1".into(),
                    updated_at: "1".into(),
                    last_opened_at: Some("1".into()),
                })
                .unwrap();
        }
        storage
            .activate_session(&SessionRecord {
                id: "interrupted-session".into(),
                work_item_id: "interrupted".into(),
                parent_id: None,
                active: true,
                created_at: "1".into(),
            })
            .unwrap();
        storage
            .begin_prune_operation("recovery-operation", "interrupted", false)
            .unwrap();
        drop(storage);
        let agent = ControlledAgent::with_config(BridgeConfig {
            work_item_id: "current".into(),
            session_root: directory.path().join("current"),
            database_path: paths.database.clone(),
            app_paths: paths.clone(),
            existing_session_id: None,
            model: "controlled-fast".into(),
            reasoning_effort: None,
            context_tier: None,
            skill_directories: Vec::new(),
            plugin_directories: Vec::new(),
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            if let Some(event) = agent.try_recv_laned() {
                let ready = matches!(
                    event.event,
                    LaneEvent::Agent(AgentEvent::SessionReady { .. })
                );
                events.push(event);
                if ready {
                    break;
                }
            } else {
                std::thread::yield_now();
            }
        }

        assert!(events.iter().any(|event| matches!(
            event.event,
            LaneEvent::Agent(AgentEvent::PruneRecoveryStarted {
                ref operation_id,
                ..
            }) if operation_id == "recovery-operation"
        )));
        assert!(events.iter().any(|event| matches!(
            event.event,
            LaneEvent::Agent(AgentEvent::PruneSessionProgress { total: 1, .. })
        )));
        assert!(events.iter().any(|event| matches!(
            event.event,
            LaneEvent::Agent(AgentEvent::PruneRecovery {
                ref outcome,
                ..
            }) if outcome.remote_deleted && outcome.local_deleted && outcome.error.is_none()
        )));
        assert!(Storage::open(&paths.database)
            .unwrap()
            .work_item_by_id("interrupted")
            .unwrap()
            .is_none());
    }

    #[test]
    fn normal_turns_use_explicit_sdk_enqueue_delivery() {
        let outbound = Outbound {
            id: "outbound".into(),
            kind: OutboundKind::Chat,
            text: "queue this".into(),
        };
        let options = enqueue_message(&outbound);
        assert_eq!(options.prompt, "queue this");
        assert_eq!(
            options.mode,
            Some(github_copilot_sdk::DeliveryMode::Enqueue)
        );
    }

    #[test]
    fn resume_explicitly_continues_pending_sdk_work() {
        let config = BridgeConfig {
            work_item_id: "work-item".into(),
            session_root: PathBuf::from("."),
            database_path: PathBuf::from("review.db"),
            app_paths: test_app_paths(PathBuf::from("/tmp/rq-tui-resume-config")),
            existing_session_id: Some("previous".into()),
            model: "controlled-fast".into(),
            reasoning_effort: Some("high".into()),
            context_tier: Some("long_context".into()),
            skill_directories: Vec::new(),
            plugin_directories: vec![PathBuf::from("plugins")],
        };
        assert_eq!(
            resume_config(SessionId::new("previous"), &config, None).continue_pending_work,
            Some(true)
        );
        assert_eq!(
            create_config(&config, None).reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(
            create_config(&config, None).context_tier.as_deref(),
            Some("long_context")
        );
        assert_eq!(
            create_config(&config, None).plugin_directories,
            Some(vec![PathBuf::from("plugins")])
        );
        assert_eq!(
            resume_config(SessionId::new("previous"), &config, None).plugin_directories,
            Some(vec![PathBuf::from("plugins")])
        );
    }

    #[tokio::test]
    async fn permission_handler_allows_reads_and_denies_shell_and_write() {
        let handler = ReadOnlyPermissionHandler;
        for kind in [PermissionRequestKind::Read] {
            let result = handler
                .handle(
                    SessionId::new("s"),
                    RequestId::new("r"),
                    PermissionRequestData {
                        kind: Some(kind),
                        ..Default::default()
                    },
                )
                .await;
            assert!(matches!(
                result,
                github_copilot_sdk::handler::PermissionResult::Decision(_)
            ));
        }
        for kind in [PermissionRequestKind::Shell, PermissionRequestKind::Write] {
            let result = handler
                .handle(
                    SessionId::new("s"),
                    RequestId::new("r"),
                    PermissionRequestData {
                        kind: Some(kind),
                        ..Default::default()
                    },
                )
                .await;
            let debug = format!("{result:?}");
            assert!(debug.contains("Reject"));
        }
        let result = handler
            .handle(
                SessionId::new("s"),
                RequestId::new("r"),
                PermissionRequestData {
                    kind: Some(PermissionRequestKind::CustomTool),
                    extra: serde_json::json!({"toolName": "write_and_read"}),
                    ..Default::default()
                },
            )
            .await;
        assert!(format!("{result:?}").contains("Reject"));
    }

    #[tokio::test]
    async fn pre_tool_progress_does_not_claim_permission_was_already_granted() {
        let (sender, receiver) = mpsc::channel();
        let hooks = ProgressHooks {
            events: EventPublisher::new(sender, AgentLane::Main),
        };
        hooks
            .on_hook(HookEvent::PreToolUse {
                input: PreToolUseInput {
                    session_id: "session".into(),
                    timestamp: 0.0,
                    working_directory: PathBuf::from("."),
                    tool_name: "read_file".into(),
                    tool_args: serde_json::json!({}),
                },
                ctx: HookContext {
                    session_id: SessionId::new("session"),
                },
            })
            .await;
        let activity = receiver
            .recv()
            .expect("pre-tool hook activity")
            .activity
            .expect("detailed pre-tool activity");
        assert_eq!(
            activity.detail.as_deref(),
            Some("Awaiting the separate read-only permission-policy decision")
        );
    }

    fn event(event_type: &str, data: serde_json::Value) -> SessionEvent {
        SessionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            parent_id: Some("test-root".into()),
            ephemeral: None,
            agent_id: None,
            debug_cli_received_at_ms: None,
            debug_ws_forwarded_at_ms: None,
            event_type: event_type.into(),
            data,
        }
    }

    fn session_event(event_type: &str, data: serde_json::Value) -> SessionEvent {
        let mut event = event(event_type, data);
        event.parent_id = None;
        event
    }

    fn active() -> Option<ActiveOutbound> {
        Some(ActiveOutbound {
            outbound: Outbound {
                id: "outbound".into(),
                kind: OutboundKind::Chat,
                text: "hello".into(),
            },
            response_started: false,
            turn_started: true,
            turn_id: Some("test-turn".into()),
            sdk_message_ids: HashSet::new(),
            accepted_event_ids: HashSet::from(["test-root".into()]),
            message_buffers: Default::default(),
            message_order: Vec::new(),
            emitted_text: String::new(),
            hidden_message_ids: Default::default(),
        })
    }

    fn drain(receiver: &mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        receiver.try_iter().collect()
    }

    #[test]
    fn documented_streaming_event_fixtures_are_visible() {
        let fixtures = [
            (
                "assistant.turn_start",
                serde_json::json!({"turnId": "test-turn"}),
                "Assistant turn started",
            ),
            (
                "assistant.turn_end",
                serde_json::json!({"turnId": "test-turn", "model": "fixture-model"}),
                "Assistant turn ended",
            ),
            (
                "assistant.streaming_delta",
                serde_json::json!({"totalResponseSizeBytes": 128}),
                "Receiving streamed response",
            ),
            (
                "session.compaction_start",
                serde_json::json!({"model": "fixture-model", "conversationTokens": 9000}),
                "Compacting conversation context",
            ),
            (
                "session.compaction_complete",
                serde_json::json!({"success": true, "tokensRemoved": 700}),
                "Conversation context compacted",
            ),
            (
                "permission.requested",
                serde_json::json!({
                    "requestId": "permission-1",
                    "permissionRequest": {"kind": "shell"}
                }),
                "Permission requested",
            ),
            (
                "permission.completed",
                serde_json::json!({"requestId": "permission-1"}),
                "Permission request resolved",
            ),
            (
                "user_input.requested",
                serde_json::json!({
                    "requestId": "input-1",
                    "question": "Which environment should I use?"
                }),
                "waiting for your input",
            ),
            (
                "user_input.completed",
                serde_json::json!({"requestId": "input-1"}),
                "User input received",
            ),
            (
                "elicitation.requested",
                serde_json::json!({
                    "requestId": "elicit-1",
                    "message": "Confirm the remote session",
                    "mode": "form"
                }),
                "waiting for confirmation",
            ),
            (
                "elicitation.completed",
                serde_json::json!({"requestId": "elicit-1", "action": "accept"}),
                "User confirmation received",
            ),
            (
                "tool.user_requested",
                serde_json::json!({"toolCallId": "tool-1", "toolName": "read_file"}),
                "User requested read_file",
            ),
            (
                "session.info",
                serde_json::json!({
                    "infoType": "remote",
                    "message": "Remote session is ready"
                }),
                "Copilot session info",
            ),
            (
                "session.warning",
                serde_json::json!({"warningType": "policy", "message": "Read-only mode"}),
                "Copilot session warning",
            ),
            (
                "session.remote_steerable_changed",
                serde_json::json!({"remoteSteerable": true}),
                "Remote session steering state changed",
            ),
            (
                "session.start",
                serde_json::json!({"sessionId": "session-1"}),
                "Copilot session started",
            ),
            (
                "session.resume",
                serde_json::json!({"sessionId": "session-1"}),
                "Copilot session resumed",
            ),
            (
                "session.model_change",
                serde_json::json!({"newModel": "fixture-model"}),
                "Copilot model_change",
            ),
            (
                "session.session_limits_changed",
                serde_json::json!({"status": "available"}),
                "Copilot session_limits_changed",
            ),
            (
                "session.context_changed",
                serde_json::json!({"status": "updated"}),
                "Copilot context_changed",
            ),
            (
                "session.usage_info",
                serde_json::json!({"currentTokens": 400, "tokenLimit": 8000}),
                "Copilot usage_info",
            ),
            (
                "session.shutdown",
                serde_json::json!({"reason": "fixture complete"}),
                "Copilot session shutting down",
            ),
        ];

        for (event_type, data, expected_label) in fixtures {
            let (sender, receiver) = mpsc::channel();
            let mut active = active();
            handle_session_event(session_event(event_type, data), &mut active, &sender);
            let emitted = drain(&receiver);
            assert!(
                emitted.iter().any(
                    |event| matches!(event, AgentEvent::Activity { label, .. } if label.contains(expected_label))
                ),
                "event {event_type} did not produce {expected_label:?}: {emitted:?}"
            );
            if event_type == "session.compaction_complete" {
                assert!(emitted
                    .iter()
                    .any(|event| matches!(event, AgentEvent::Compacted)));
            }
        }
    }

    #[test]
    fn unknown_events_preserve_type_and_sanitize_detail() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            session_event(
                "vendor.future_signal",
                serde_json::json!({
                    "content": "private reasoning must not be shown",
                    "arguments": {"token": "secret"},
                    "message": "queued\nnow"
                }),
            ),
            &mut active,
            &sender,
        );
        assert!(matches!(
            drain(&receiver).as_slice(),
            [AgentEvent::Activity { label, .. }]
                if label == "SDK event vendor.future_signal · message=queued now"
        ));
    }

    #[test]
    fn documented_event_fixtures_tolerate_malformed_payloads() {
        let event_types = [
            "assistant.turn_start",
            "assistant.turn_end",
            "assistant.streaming_delta",
            "session.compaction_start",
            "session.compaction_complete",
            "permission.requested",
            "permission.completed",
            "user_input.requested",
            "user_input.completed",
            "elicitation.requested",
            "elicitation.completed",
            "tool.user_requested",
            "session.info",
            "session.warning",
            "session.remote_steerable_changed",
            "session.start",
            "session.resume",
            "session.model_change",
            "session.session_limits_changed",
            "session.context_changed",
            "session.usage_info",
            "session.shutdown",
            "vendor.malformed",
        ];
        let malformed_payloads = [
            serde_json::Value::Null,
            serde_json::json!("not-an-object"),
            serde_json::json!([]),
            serde_json::json!({}),
        ];

        for event_type in event_types {
            for data in malformed_payloads.clone() {
                let (sender, receiver) = mpsc::channel();
                let mut active = active();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_session_event(session_event(event_type, data), &mut active, &sender);
                }));
                assert!(
                    result.is_ok(),
                    "normalizing malformed event {event_type} panicked"
                );
                let _ = drain(&receiver);
            }
        }
    }

    #[test]
    fn streaming_deltas_are_not_duplicated_by_the_final_message() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            event(
                "assistant.message_delta",
                serde_json::json!({"messageId": "m1", "deltaContent": "Hel"}),
            ),
            &mut active,
            &sender,
        );
        handle_session_event(
            event(
                "assistant.message_delta",
                serde_json::json!({"messageId": "m1", "deltaContent": "lo"}),
            ),
            &mut active,
            &sender,
        );
        handle_session_event(
            event(
                "assistant.message",
                serde_json::json!({"messageId": "m1", "content": "Hello"}),
            ),
            &mut active,
            &sender,
        );
        assert_eq!(
            drain(&receiver),
            vec![
                AgentEvent::ResponseStarted {
                    outbound_id: "outbound".into(),
                    outbound: OutboundKind::Chat,
                    first_delta: "Hel".into(),
                },
                AgentEvent::ResponseDelta {
                    outbound_id: "outbound".into(),
                    delta: "lo".into(),
                },
            ]
        );
    }

    #[test]
    fn final_message_resynchronizes_after_a_dropped_delta() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            event(
                "assistant.message_delta",
                serde_json::json!({"messageId": "m1", "deltaContent": "Hel"}),
            ),
            &mut active,
            &sender,
        );
        handle_session_event(
            event(
                "assistant.message",
                serde_json::json!({"messageId": "m1", "content": "Hello"}),
            ),
            &mut active,
            &sender,
        );
        assert!(matches!(
            drain(&receiver).as_slice(),
            [
                AgentEvent::ResponseStarted { .. },
                AgentEvent::ResponseSnapshot { text, .. }
            ] if text == "Hello"
        ));
    }

    #[test]
    fn final_only_agentic_phases_are_all_preserved() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        for (id, content) in [("m1", "I will inspect."), ("m2", "The answer is 42.")] {
            handle_session_event(
                event(
                    "assistant.message",
                    serde_json::json!({"messageId": id, "content": content}),
                ),
                &mut active,
                &sender,
            );
        }
        assert!(matches!(
            drain(&receiver).last(),
            Some(AgentEvent::ResponseSnapshot { text, .. })
                if text == "I will inspect.\n\nThe answer is 42."
        ));
    }

    #[test]
    fn subagent_text_does_not_leak_into_the_parent_chat() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        let mut subagent = event(
            "assistant.message_delta",
            serde_json::json!({"messageId": "m1", "deltaContent": "internal"}),
        );
        subagent.agent_id = Some("subagent".into());
        handle_session_event(subagent, &mut active, &sender);
        assert!(drain(&receiver).is_empty());
    }

    #[test]
    fn thinking_phase_text_is_kept_out_of_visible_chat() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            event(
                "assistant.message_start",
                serde_json::json!({"messageId": "thinking", "phase": "thinking"}),
            ),
            &mut active,
            &sender,
        );
        handle_session_event(
            event(
                "assistant.message_delta",
                serde_json::json!({
                    "messageId": "thinking",
                    "deltaContent": "private reasoning"
                }),
            ),
            &mut active,
            &sender,
        );
        assert!(drain(&receiver).is_empty());
    }

    #[test]
    fn resumed_history_uses_display_content_and_hides_grounding_envelopes() {
        let history = history_entries(&[
            event(
                "user.message",
                serde_json::json!({
                    "content": "Code review ask\nAnnotation: a\nQuestion: Why this timeout?"
                }),
            ),
            event(
                "assistant.message",
                serde_json::json!({"content": "It limits the retry window."}),
            ),
        ]);
        assert_eq!(
            history,
            vec![
                HistoryEntry {
                    role: "you".into(),
                    text: "Why this timeout?".into(),
                },
                HistoryEntry {
                    role: "copilot".into(),
                    text: "It limits the retry window.".into(),
                },
            ]
        );
    }

    #[test]
    fn resumed_in_flight_turn_gets_a_synthetic_local_outbound() {
        let history = vec![
            event(
                "user.message",
                serde_json::json!({"content": "continue this review"}),
            ),
            event(
                "assistant.turn_start",
                serde_json::json!({"turnId": "turn-7"}),
            ),
        ];
        let active = resumed_active_from_history(&history, &SessionId::new("session"))
            .expect("pending resumed turn");
        assert!(active.turn_started);
        assert_eq!(active.turn_id.as_deref(), Some("turn-7"));
        assert!(active.outbound.id.starts_with("resumed:session:"));

        let mut completed = history;
        completed.push(event(
            "assistant.turn_end",
            serde_json::json!({"turnId": "turn-7"}),
        ));
        assert!(resumed_active_from_history(&completed, &SessionId::new("session")).is_none());
    }

    #[test]
    fn stale_events_are_quarantined_until_the_new_turn_starts() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        active.as_mut().unwrap().turn_started = false;
        active.as_mut().unwrap().turn_id = None;
        handle_session_event(
            event(
                "assistant.message_delta",
                serde_json::json!({"messageId": "old", "deltaContent": "stale"}),
            ),
            &mut active,
            &sender,
        );
        handle_session_event(
            event("session.idle", serde_json::json!({"aborted": false})),
            &mut active,
            &sender,
        );
        assert!(active.is_some());
        assert!(drain(&receiver)
            .iter()
            .all(|event| !matches!(event, AgentEvent::ResponseDelta { .. })));

        handle_session_event(
            event(
                "assistant.turn_start",
                serde_json::json!({"turnId": "new-turn"}),
            ),
            &mut active,
            &sender,
        );
        handle_session_event(
            event(
                "assistant.message_delta",
                serde_json::json!({"messageId": "new", "deltaContent": "fresh"}),
            ),
            &mut active,
            &sender,
        );
        assert!(drain(&receiver).iter().any(
            |event| matches!(event, AgentEvent::ResponseStarted { first_delta, .. } if first_delta == "fresh")
        ));
    }

    #[test]
    fn sdk_acknowledgement_binds_through_the_matching_user_event() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        let active_turn = active.as_mut().unwrap();
        active_turn.turn_started = false;
        active_turn.turn_id = None;
        active_turn.sdk_message_ids = HashSet::from(["interaction-new".into()]);
        active_turn.accepted_event_ids.clear();

        let stale_user = event(
            "user.message",
            serde_json::json!({
                "content": "an older prompt",
                "interactionId": "interaction-old"
            }),
        );
        let stale_user_id = stale_user.id.clone();
        handle_session_event(stale_user, &mut active, &sender);
        let mut stale_start = event(
            "assistant.turn_start",
            serde_json::json!({"turnId": "old-turn"}),
        );
        stale_start.parent_id = Some(stale_user_id);
        handle_session_event(stale_start, &mut active, &sender);
        assert!(!active.as_ref().unwrap().turn_started);

        let matching_user = event(
            "user.message",
            serde_json::json!({
                "content": "hello",
                "interactionId": "interaction-new"
            }),
        );
        let matching_user_id = matching_user.id.clone();
        handle_session_event(matching_user, &mut active, &sender);
        let mut matching_start = event(
            "assistant.turn_start",
            serde_json::json!({"turnId": "new-turn"}),
        );
        let matching_start_id = matching_start.id.clone();
        matching_start.parent_id = Some(matching_user_id);
        handle_session_event(matching_start, &mut active, &sender);
        let mut matching_delta = event(
            "assistant.message_delta",
            serde_json::json!({"messageId": "new", "deltaContent": "fresh"}),
        );
        matching_delta.parent_id = Some(matching_start_id);
        handle_session_event(matching_delta, &mut active, &sender);

        assert!(active.as_ref().unwrap().turn_started);
        assert!(drain(&receiver).iter().any(
            |event| matches!(event, AgentEvent::ResponseStarted { first_delta, .. } if first_delta == "fresh")
        ));
    }

    #[test]
    fn immediate_steering_message_root_binds_its_descendant_chain() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        register_sdk_message_root(active.as_mut().unwrap(), "steer-interaction".into());

        let steering_root = event(
            "user.message",
            serde_json::json!({
                "content": "focus on the failing test",
                "interactionId": "steer-interaction"
            }),
        );
        let steering_root_id = steering_root.id.clone();
        handle_session_event(steering_root, &mut active, &sender);
        assert!(active
            .as_ref()
            .unwrap()
            .accepted_event_ids
            .contains(&steering_root_id));

        let mut turn_start = event(
            "assistant.turn_start",
            serde_json::json!({"turnId": "steered-turn"}),
        );
        let turn_start_id = turn_start.id.clone();
        turn_start.parent_id = Some(steering_root_id);
        handle_session_event(turn_start, &mut active, &sender);

        let mut delta = event(
            "assistant.message_delta",
            serde_json::json!({"messageId": "steered", "deltaContent": "updated"}),
        );
        delta.parent_id = Some(turn_start_id);
        handle_session_event(delta, &mut active, &sender);

        assert!(matches!(
            drain(&receiver).last(),
            Some(AgentEvent::ResponseStarted { first_delta, .. }) if first_delta == "updated"
        ));
    }

    #[test]
    fn sdk_steering_message_id_is_also_accepted_as_a_direct_chain_root() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        register_sdk_message_root(active.as_mut().unwrap(), "steer-message-id".into());

        let mut turn_start = event(
            "assistant.turn_start",
            serde_json::json!({"turnId": "steered-turn"}),
        );
        let turn_start_id = turn_start.id.clone();
        turn_start.parent_id = Some("steer-message-id".into());
        handle_session_event(turn_start, &mut active, &sender);

        let mut delta = event(
            "assistant.message_delta",
            serde_json::json!({"messageId": "steered", "deltaContent": "direct root"}),
        );
        delta.parent_id = Some(turn_start_id);
        handle_session_event(delta, &mut active, &sender);

        assert!(matches!(
            drain(&receiver).last(),
            Some(AgentEvent::ResponseStarted { first_delta, .. }) if first_delta == "direct root"
        ));
    }

    #[test]
    fn late_events_from_an_old_chain_cannot_complete_the_new_turn() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        active.as_mut().unwrap().turn_started = false;
        active.as_mut().unwrap().turn_id = None;

        let start = event(
            "assistant.turn_start",
            serde_json::json!({"turnId": "new-turn"}),
        );
        let start_id = start.id.clone();
        handle_session_event(start, &mut active, &sender);

        let mut stale_delta = event(
            "assistant.message_delta",
            serde_json::json!({"messageId": "old", "deltaContent": "wrong turn"}),
        );
        stale_delta.parent_id = Some("old-turn-chain".into());
        handle_session_event(stale_delta, &mut active, &sender);
        let mut stale_idle = event("session.idle", serde_json::json!({"aborted": false}));
        stale_idle.parent_id = Some("old-turn-chain".into());
        handle_session_event(stale_idle, &mut active, &sender);
        assert!(active.is_some());

        let mut fresh_delta = event(
            "assistant.message_delta",
            serde_json::json!({"messageId": "new", "deltaContent": "right turn"}),
        );
        fresh_delta.parent_id = Some(start_id);
        handle_session_event(fresh_delta, &mut active, &sender);
        let events = drain(&receiver);
        assert!(events.iter().any(
            |event| matches!(event, AgentEvent::ResponseStarted { first_delta, .. } if first_delta == "right turn")
        ));
        assert!(events.iter().all(|event| {
            !matches!(event, AgentEvent::ResponseStarted { first_delta, .. } if first_delta == "wrong turn")
        }));
    }

    #[test]
    fn aborted_idle_completes_and_releases_the_active_turn() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            session_event("session.idle", serde_json::json!({"aborted": true})),
            &mut active,
            &sender,
        );
        assert!(active.is_none());
        assert!(matches!(
            drain(&receiver).as_slice(),
            [
                AgentEvent::ResponseStarted { .. },
                AgentEvent::ResponseComplete { aborted: true, .. }
            ]
        ));
    }

    #[test]
    fn idle_with_background_tasks_keeps_the_turn_active_until_truly_idle() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            session_event(
                "session.idle",
                serde_json::json!({
                    "backgroundTasks": {"agents": [{"id": "research"}], "shells": []}
                }),
            ),
            &mut active,
            &sender,
        );
        assert!(active.is_some());
        assert!(matches!(
            drain(&receiver).as_slice(),
            [AgentEvent::Activity { label, .. }]
                if label == "Copilot is still running background work"
        ));

        handle_session_event(
            session_event(
                "session.idle",
                serde_json::json!({"backgroundTasks": {"agents": [], "shells": []}}),
            ),
            &mut active,
            &sender,
        );
        assert!(active.is_none());
        assert!(matches!(
            drain(&receiver).as_slice(),
            [
                AgentEvent::ResponseStarted { .. },
                AgentEvent::ResponseComplete { aborted: false, .. }
            ]
        ));
    }

    #[test]
    fn task_complete_is_visible_but_does_not_replace_the_idle_boundary() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            session_event(
                "session.task_complete",
                serde_json::json!({"summary": "The requested review is complete."}),
            ),
            &mut active,
            &sender,
        );
        assert!(active.is_some());
        assert!(matches!(
            drain(&receiver).as_slice(),
            [AgentEvent::Activity { label, .. }]
                if label == "Copilot marked the task complete"
        ));
    }

    #[test]
    fn unparented_session_error_fails_the_active_turn() {
        let (sender, receiver) = mpsc::channel();
        let mut active = active();
        handle_session_event(
            session_event(
                "session.error",
                serde_json::json!({"message": "session-level controlled failure"}),
            ),
            &mut active,
            &sender,
        );

        assert!(active.is_none());
        assert!(matches!(
            drain(&receiver).as_slice(),
            [AgentEvent::TurnFailed { message, .. }]
                if message == "session-level controlled failure"
        ));
    }

    #[test]
    fn controlled_agent_exposes_typed_ephemeral_side_lifecycle() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned(); // SessionReady.

        agent
            .send(AgentCommand::StartSide { outbound: None })
            .expect("start side");
        let started = agent.try_recv_laned().expect("side lifecycle event");
        assert!(matches!(
            started,
            super::AgentEventEnvelope {
                lane: AgentLane::Side { ref id },
                event: LaneEvent::SideStarted { ref side_id, .. },
                activity: None,
            } if id == side_id && side_id == "1"
        ));
        let ready = agent.try_recv_laned().expect("side ready activity");
        assert!(matches!(
            ready,
            super::AgentEventEnvelope {
                lane: AgentLane::Side { .. },
                activity: Some(ref activity),
                ..
            } if activity.label.contains("MAIN history is unchanged")
        ));

        agent.send(AgentCommand::ExitSide).expect("exit side");
        let exited = agent.try_recv_laned().expect("side exit lifecycle event");
        assert!(matches!(
            exited.event,
            LaneEvent::SideExited { ref side_id, .. } if side_id == "1"
        ));
    }

    #[test]
    fn controlled_side_exit_interrupts_an_active_turn_immediately() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned();
        agent
            .send(AgentCommand::StartSide {
                outbound: Some(Outbound::new(
                    OutboundKind::Chat,
                    "long side question".into(),
                )),
            })
            .expect("start active side");
        assert!(matches!(
            agent.try_recv_laned().map(|envelope| envelope.event),
            Some(LaneEvent::SideStarted { .. })
        ));

        agent
            .send(AgentCommand::ExitSide)
            .expect("exit active side");
        let exited = agent.try_recv_laned().expect("immediate side exit");
        assert!(matches!(exited.event, LaneEvent::SideExited { .. }));
        assert!(agent.try_recv_laned().is_none());
    }

    #[test]
    fn controlled_queue_cancellation_stays_on_the_outbound_lane() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned();
        agent
            .send(AgentCommand::StartSide { outbound: None })
            .expect("start side");
        let _ = agent.try_recv_laned();
        let _ = agent.try_recv_laned();

        let outbound = Outbound::new(OutboundKind::Chat, "main queue item".into());
        let outbound_id = outbound.id.clone();
        agent
            .send(AgentCommand::Send(outbound))
            .expect("queue main");
        agent
            .send(AgentCommand::CancelQueued(outbound_id.clone()))
            .expect("cancel main");

        let events = std::iter::from_fn(|| agent.try_recv_laned()).collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                super::AgentEventEnvelope {
                    lane: AgentLane::Main,
                    event: LaneEvent::Agent(AgentEvent::QueueCancelled { outbound_id: id }),
                    ..
                } if id == &outbound_id
            )
        }));
    }

    #[test]
    fn controlled_abort_removes_all_late_success_events_for_the_outbound() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned();
        let outbound = Outbound::new(OutboundKind::Chat, "cancel before late delta".into());
        let outbound_id = outbound.id.clone();
        agent
            .send(AgentCommand::Send(outbound))
            .expect("schedule controlled turn");
        agent
            .send(AgentCommand::Abort)
            .expect("abort controlled turn");

        let state = agent.state.lock().expect("controlled agent lock");
        assert!(state.events.iter().all(|(_, envelope)| {
            if envelope_outbound_id(envelope).as_deref() != Some(outbound_id.as_str()) {
                return true;
            }
            !matches!(
                &envelope.event,
                LaneEvent::Agent(
                    AgentEvent::ResponseDelta { .. }
                        | AgentEvent::ResponseComplete { aborted: false, .. }
                )
            )
        }));
        assert!(state.events.iter().any(|(_, envelope)| {
            matches!(
                &envelope.event,
                LaneEvent::Agent(AgentEvent::ResponseComplete {
                    outbound_id: id,
                    aborted: true,
                }) if id == &outbound_id
            )
        }));
    }

    #[test]
    fn controlled_abort_advances_the_next_queued_turn_immediately() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned();
        let first = Outbound::new(OutboundKind::Chat, "first".into());
        let first_id = first.id.clone();
        let second = Outbound::new(OutboundKind::Chat, "second".into());
        let second_id = second.id.clone();
        agent.send(AgentCommand::Send(first)).unwrap();
        agent.send(AgentCommand::Send(second)).unwrap();
        agent.send(AgentCommand::Abort).unwrap();

        let state = agent.state.lock().expect("controlled agent lock");
        assert!(state.events.iter().all(|(_, envelope)| {
            if envelope_outbound_id(envelope).as_deref() != Some(first_id.as_str()) {
                return true;
            }
            matches!(
                &envelope.event,
                LaneEvent::Agent(
                    AgentEvent::Activity { .. }
                        | AgentEvent::ResponseComplete { aborted: true, .. }
                )
            )
        }));
        let next_at = state
            .events
            .iter()
            .filter(|(_, envelope)| {
                envelope_outbound_id(envelope).as_deref() == Some(second_id.as_str())
            })
            .map(|(scheduled, _)| *scheduled)
            .min()
            .expect("second turn remains scheduled");
        assert!(
            next_at.saturating_duration_since(Instant::now()) < Duration::from_millis(100),
            "queued continuation should not inherit the cancelled turn's old deadline"
        );
    }

    #[test]
    fn controlled_queue_replacement_stays_on_the_outbound_lane() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned();
        agent
            .send(AgentCommand::StartSide { outbound: None })
            .expect("start side");
        let _ = agent.try_recv_laned();
        let _ = agent.try_recv_laned();

        let original = Outbound::new(OutboundKind::Chat, "main queue item".into());
        let original_id = original.id.clone();
        agent
            .send(AgentCommand::Send(original))
            .expect("queue main");
        let replacement = Outbound::new(OutboundKind::Chat, "replacement".into());
        let replacement_id = replacement.id.clone();
        agent
            .send(AgentCommand::ReplaceQueued {
                outbound_id: original_id.clone(),
                replacement,
            })
            .expect("replace main");

        let event = agent.try_recv_laned().expect("replacement acknowledgement");
        assert!(matches!(
            event,
            super::AgentEventEnvelope {
                lane: AgentLane::Main,
                event: LaneEvent::Agent(AgentEvent::QueueReplaced {
                    outbound_id,
                    replacement_id: actual_replacement_id,
                    ..
                }),
                ..
            } if outbound_id == original_id && actual_replacement_id == replacement_id
        ));
    }

    #[test]
    fn controlled_queue_rejection_stays_on_the_original_lane() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned();
        let original = Outbound::new(OutboundKind::Chat, "main queue item".into());
        let original_id = original.id.clone();
        agent
            .send(AgentCommand::Send(original))
            .expect("queue main");
        let _ = agent.try_recv_laned(); // The item has left its cancellable queue.

        agent
            .send(AgentCommand::StartSide { outbound: None })
            .expect("start side");
        let _ = agent.try_recv_laned();
        let _ = agent.try_recv_laned();

        let replacement = Outbound::new(OutboundKind::Chat, "replacement".into());
        let replacement_id = replacement.id.clone();
        agent
            .send(AgentCommand::ReplaceQueued {
                outbound_id: original_id.clone(),
                replacement,
            })
            .expect("replace rejected main item");

        let event = agent.try_recv_laned().expect("replacement rejection");
        assert!(matches!(
            event,
            super::AgentEventEnvelope {
                lane: AgentLane::Main,
                event: LaneEvent::Agent(AgentEvent::QueueReplaceRejected {
                    outbound_id,
                    replacement_id: actual_replacement_id,
                    ..
                }),
                ..
            } if outbound_id == original_id && actual_replacement_id == replacement_id
        ));
    }

    #[test]
    fn controlled_steering_is_correlated_to_the_actual_outbound() {
        let agent = ControlledAgent::new("work-item".into());
        let _ = agent.try_recv_laned();
        let outbound = Outbound::new(OutboundKind::Chat, "main work".into());
        let outbound_id = outbound.id.clone();
        agent
            .send(AgentCommand::Send(outbound))
            .expect("queue main");
        let steering = Outbound::new(OutboundKind::Correction, "focus on the failure".into());
        let steering_id = steering.id.clone();
        agent
            .send(AgentCommand::Steer(steering))
            .expect("steer main");

        let events = std::iter::from_fn(|| agent.try_recv_laned()).collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                super::AgentEventEnvelope {
                    lane: AgentLane::Main,
                    event: LaneEvent::Agent(AgentEvent::SteeringAccepted {
                        steering_id: actual_steering_id,
                        active_outbound_id,
                    }),
                    activity: None,
                } if actual_steering_id == &steering_id
                    && active_outbound_id == &outbound_id
            )
        }));
    }

    #[test]
    fn queue_positions_only_count_an_active_turn_in_its_own_lane() {
        let main_queue = VecDeque::from([Outbound::new(OutboundKind::Chat, "main".into())]);
        let side_queue = VecDeque::from([Outbound::new(OutboundKind::Chat, "side".into())]);
        let active = active();
        let side_lane = AgentLane::Side {
            id: "side-1".into(),
        };

        assert_eq!(
            super::queue_position(&main_queue, &active, &side_lane, &AgentLane::Main),
            1
        );
        assert_eq!(
            super::queue_position(&side_queue, &active, &side_lane, &side_lane),
            2
        );
    }

    #[test]
    fn worker_fatal_events_follow_the_last_visible_lane() {
        let current_lane = Arc::new(std::sync::Mutex::new(AgentLane::Main));
        let side = AgentLane::Side {
            id: "side-1".into(),
        };
        super::set_current_lane(&current_lane, side.clone());
        assert_eq!(super::tracked_lane(&current_lane), side);
    }

    #[test]
    fn lane_publisher_preserves_tool_metadata_for_side_events() {
        let (sender, receiver) = mpsc::channel();
        let publisher = EventPublisher::new(
            sender,
            AgentLane::Side {
                id: "diagnostic".into(),
            },
        );
        let mut active = active();
        handle_session_event(
            event(
                "tool.execution_progress",
                serde_json::json!({
                    "toolName": "run_skill",
                    "progressMessage": "Running security-review skill"
                }),
            ),
            &mut active,
            &publisher,
        );
        let envelope = receiver.recv().expect("tool activity");
        assert!(matches!(
            envelope,
            super::AgentEventEnvelope {
                lane: AgentLane::Side { ref id },
                activity: Some(ref activity),
                ..
            } if id == "diagnostic"
                && activity.kind == ActivityKind::ToolProgress
                && activity.tool.as_deref() == Some("run_skill")
                && activity.detail.as_deref() == Some("Running security-review skill")
        ));
    }

    #[test]
    fn raw_tool_completion_error_is_typed_as_failure_with_sdk_diagnostics() {
        let (sender, receiver) = mpsc::channel();
        let publisher = EventPublisher::new(sender, AgentLane::Main);
        let mut active = active();
        handle_session_event(
            event(
                "tool.execution_complete",
                serde_json::json!({
                    "toolName": "read_file",
                    "success": false,
                    "error": {
                        "code": "E_PERMISSION",
                        "message": "permission denied"
                    }
                }),
            ),
            &mut active,
            &publisher,
        );

        let envelope = receiver.recv().expect("tool failure activity");
        assert!(matches!(
            envelope,
            super::AgentEventEnvelope {
                lane: AgentLane::Main,
                activity: Some(ref activity),
                ..
            } if activity.kind == ActivityKind::Failure
                && activity.label == "Failed read_file"
                && activity.tool.as_deref() == Some("read_file")
                && activity.detail.as_deref() == Some("E_PERMISSION: permission denied")
        ));
    }

    #[test]
    fn raw_tool_completion_result_error_is_typed_as_failure_with_result_details() {
        let (sender, receiver) = mpsc::channel();
        let publisher = EventPublisher::new(sender, AgentLane::Main);
        let mut active = active();
        handle_session_event(
            event(
                "tool.execution_complete",
                serde_json::json!({
                    "toolName": "search",
                    "success": true,
                    "result": {
                        "isError": true,
                        "content": "search backend unavailable"
                    }
                }),
            ),
            &mut active,
            &publisher,
        );

        let envelope = receiver.recv().expect("result error activity");
        assert!(matches!(
            envelope,
            super::AgentEventEnvelope {
                lane: AgentLane::Main,
                activity: Some(ref activity),
                ..
            } if activity.kind == ActivityKind::Failure
                && activity.label == "Failed search"
                && activity.tool.as_deref() == Some("search")
                && activity.detail.as_deref() == Some("search backend unavailable")
        ));
    }

    #[test]
    fn raw_successful_tool_completion_remains_tool_complete() {
        let (sender, receiver) = mpsc::channel();
        let publisher = EventPublisher::new(sender, AgentLane::Main);
        let mut active = active();
        handle_session_event(
            event(
                "tool.execution_complete",
                serde_json::json!({
                    "toolName": "read_file",
                    "success": true,
                    "result": {"content": "file contents"}
                }),
            ),
            &mut active,
            &publisher,
        );

        let envelope = receiver.recv().expect("tool completion activity");
        assert!(matches!(
            envelope,
            super::AgentEventEnvelope {
                lane: AgentLane::Main,
                activity: Some(ref activity),
                ..
            } if activity.kind == ActivityKind::ToolComplete
                && activity.label == "Finished read_file"
                && activity.tool.as_deref() == Some("read_file")
                && activity.detail.is_none()
        ));
    }

    #[tokio::test]
    async fn post_tool_use_failure_is_failure_when_no_retry_is_scheduled() {
        let (sender, receiver) = mpsc::channel();
        let hooks = ProgressHooks {
            events: EventPublisher::new(sender, AgentLane::Main),
        };
        hooks
            .on_hook(HookEvent::PostToolUseFailure {
                input: PostToolUseFailureInput {
                    session_id: "session".into(),
                    timestamp: 0.0,
                    working_directory: PathBuf::from("."),
                    tool_name: "run_command".into(),
                    tool_args: serde_json::json!({"command": "false"}),
                    error: "exit status 1".into(),
                },
                ctx: HookContext {
                    session_id: SessionId::new("session"),
                },
            })
            .await;

        let envelope = receiver.recv().expect("hook failure activity");
        assert!(matches!(
            envelope,
            super::AgentEventEnvelope {
                lane: AgentLane::Main,
                activity: Some(ref activity),
                ..
            } if activity.kind == ActivityKind::Failure
                && activity.label == "Hook: run_command failed"
                && activity.tool.as_deref() == Some("run_command")
                && activity.detail.as_deref() == Some("exit status 1")
        ));
    }

    #[test]
    fn raw_subagent_failure_is_typed_and_keeps_its_diagnostics() {
        let (sender, receiver) = mpsc::channel();
        let publisher = EventPublisher::new(sender, AgentLane::Main);
        let mut active = active();
        handle_session_event(
            event(
                "subagent.failed",
                serde_json::json!({
                    "agentDisplayName": "Edge auditor",
                    "error": "controlled subagent failure"
                }),
            ),
            &mut active,
            &publisher,
        );

        let envelope = receiver.recv().expect("subagent failure activity");
        assert!(matches!(
            envelope,
            super::AgentEventEnvelope {
                lane: AgentLane::Main,
                activity: Some(ref activity),
                ..
            } if activity.kind == ActivityKind::Failure
                && activity.label == "Subagent Edge auditor failed"
                && activity.tool.as_deref() == Some("subagent:Edge auditor")
                && activity.detail.as_deref() == Some("controlled subagent failure")
        ));
    }

    #[test]
    #[ignore = "requires an authenticated Copilot CLI; see README.md"]
    fn live_copilot_streams_and_resumes_persisted_history() {
        assert_eq!(
            env::var("RQ_TUI_LIVE_COPILOT").as_deref(),
            Ok("1"),
            "set RQ_TUI_LIVE_COPILOT=1 to acknowledge this networked test"
        );
        let model = env::var("RQ_TUI_LIVE_MODEL").unwrap_or_else(|_| "gpt-5".into());
        let live_state = tempfile::tempdir().expect("live test state directory");
        let config = BridgeConfig {
            work_item_id: format!("live-test-{}", uuid::Uuid::new_v4()),
            session_root: env::current_dir().expect("current directory"),
            database_path: live_state.path().join("review.db"),
            app_paths: test_app_paths(live_state.path().to_path_buf()),
            existing_session_id: None,
            model,
            reasoning_effort: None,
            context_tier: None,
            skill_directories: Vec::new(),
            plugin_directories: Vec::new(),
        };
        let storage = Storage::open(&config.database_path).expect("live test storage");
        storage
            .upsert_work_item(&WorkItem {
                id: config.work_item_id.clone(),
                name: "live Copilot test".into(),
                workspace_root: config.session_root.clone(),
                created_at: now(),
                updated_at: now(),
                last_opened_at: Some(now()),
            })
            .expect("seed live test Work Item");
        let bridge = CopilotBridge::start(config.clone());
        let session_id = wait_for_session(&bridge, false);
        let prompt = "Write the integers 1 through 30, separated by spaces, then write \
                      LIVE_STREAM_OK. Use no tools and add nothing else.";
        bridge
            .send(AgentCommand::Send(Outbound::new(
                OutboundKind::Chat,
                prompt.into(),
            )))
            .expect("queue live prompt");

        let deadline = Instant::now() + Duration::from_secs(120);
        let mut response = String::new();
        let mut streaming_frames = 0;
        let mut complete = false;
        while Instant::now() < deadline {
            if let Some(event) = bridge.try_recv() {
                match event {
                    AgentEvent::ResponseStarted { first_delta, .. } => {
                        if !first_delta.is_empty() {
                            streaming_frames += 1;
                            response.push_str(&first_delta);
                        }
                    }
                    AgentEvent::ResponseDelta { delta, .. } => {
                        streaming_frames += 1;
                        response.push_str(&delta);
                    }
                    AgentEvent::ResponseSnapshot { text, .. } => response = text,
                    AgentEvent::ResponseComplete { aborted, .. } => {
                        assert!(!aborted, "live response was unexpectedly aborted");
                        complete = true;
                        break;
                    }
                    AgentEvent::TurnFailed { message, .. } | AgentEvent::Error(message) => {
                        panic!("live Copilot turn failed: {message}")
                    }
                    _ => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(complete, "live Copilot turn timed out");
        assert!(
            streaming_frames > 1,
            "expected multiple ephemeral streaming frames, got {streaming_frames}"
        );
        assert!(
            response.contains("LIVE_STREAM_OK"),
            "unexpected live response: {response:?}"
        );

        bridge
            .send(AgentCommand::StartSide {
                outbound: Some(Outbound::new(
                    OutboundKind::Chat,
                    "Reply with SIDE_ONLY_TOKEN and nothing else.".into(),
                )),
            })
            .expect("start live side conversation");
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut side_started = false;
        let mut side_response = String::new();
        let mut side_complete = false;
        while Instant::now() < deadline && !side_complete {
            if let Some(envelope) = bridge.try_recv_laned() {
                match envelope.event {
                    LaneEvent::SideStarted { .. } => side_started = true,
                    LaneEvent::Agent(AgentEvent::ResponseStarted { first_delta, .. }) => {
                        assert!(matches!(envelope.lane, AgentLane::Side { .. }));
                        side_response.push_str(&first_delta);
                    }
                    LaneEvent::Agent(AgentEvent::ResponseDelta { delta, .. }) => {
                        assert!(matches!(envelope.lane, AgentLane::Side { .. }));
                        side_response.push_str(&delta);
                    }
                    LaneEvent::Agent(AgentEvent::ResponseSnapshot { text, .. }) => {
                        assert!(matches!(envelope.lane, AgentLane::Side { .. }));
                        side_response = text;
                    }
                    LaneEvent::Agent(AgentEvent::ResponseComplete { aborted, .. }) => {
                        assert!(matches!(envelope.lane, AgentLane::Side { .. }));
                        assert!(!aborted, "live side response was unexpectedly aborted");
                        side_complete = true;
                    }
                    LaneEvent::Agent(AgentEvent::TurnFailed { message, .. })
                    | LaneEvent::Agent(AgentEvent::Error(message)) => {
                        panic!("live SIDE turn failed: {message}")
                    }
                    _ => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(side_started, "SIDE lifecycle event was not emitted");
        assert!(side_complete, "live SIDE turn timed out");
        assert!(
            side_response.contains("SIDE_ONLY_TOKEN"),
            "unexpected live SIDE response: {side_response:?}"
        );
        bridge
            .send(AgentCommand::ExitSide)
            .expect("exit live side conversation");
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut side_exited = false;
        while Instant::now() < deadline && !side_exited {
            if let Some(envelope) = bridge.try_recv_laned() {
                side_exited = matches!(envelope.event, LaneEvent::SideExited { .. });
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(side_exited, "SIDE exit lifecycle event was not emitted");
        drop(bridge);

        let resumed = CopilotBridge::start(BridgeConfig {
            existing_session_id: Some(session_id),
            ..config
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut did_resume = false;
        let mut restored = Vec::new();
        while Instant::now() < deadline && (!did_resume || restored.is_empty()) {
            if let Some(event) = resumed.try_recv() {
                match event {
                    AgentEvent::SessionReady {
                        resumed,
                        resume_warning,
                        ..
                    } => {
                        assert!(resume_warning.is_none(), "{resume_warning:?}");
                        did_resume = resumed;
                    }
                    AgentEvent::HistoryLoaded(history) => restored = history,
                    AgentEvent::Error(message) => panic!("resume failed: {message}"),
                    _ => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(
            did_resume,
            "Copilot session was replaced instead of resumed"
        );
        assert!(
            restored
                .iter()
                .any(|entry| entry.role == "copilot" && entry.text.contains("LIVE_STREAM_OK")),
            "persisted response was not restored: {restored:?}"
        );
        assert!(
            restored
                .iter()
                .all(|entry| !entry.text.contains("SIDE_ONLY_TOKEN")),
            "ephemeral SIDE content leaked into MAIN history: {restored:?}"
        );
    }

    fn wait_for_session(bridge: &CopilotBridge, expected_resumed: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Some(event) = bridge.try_recv() {
                match event {
                    AgentEvent::SessionReady {
                        session_id,
                        resumed,
                        resume_warning,
                    } => {
                        assert_eq!(resumed, expected_resumed);
                        assert!(resume_warning.is_none(), "{resume_warning:?}");
                        return session_id;
                    }
                    AgentEvent::Error(message) => panic!("Copilot did not start: {message}"),
                    _ => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        panic!("Copilot session startup timed out")
    }
}

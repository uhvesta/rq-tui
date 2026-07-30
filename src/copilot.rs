use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

const SIDE_BOUNDARY: &str = "Side conversation boundary.\n\
Everything before this boundary is inherited MAIN history and is reference context only, not the \
current task. Answer only the side question below. Do not continue plans or instructions from MAIN. \
This SIDE conversation is ephemeral and read-only; do not modify files or workspace state.";
const SDK_CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

fn apply_side_boundary(outbound: &mut Outbound) {
    outbound.text = format!("{SIDE_BOUNDARY}\n\nSide question:\n{}", outbound.text);
}

#[derive(Clone, Debug)]
pub(crate) struct BridgeConfig {
    pub(crate) work_item_id: String,
    pub(crate) session_root: PathBuf,
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
            id: Uuid::new_v4().to_string(),
            kind,
            text,
        }
    }
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
    /// Legacy one-stage model switch retained while callers migrate to
    /// `SelectModel`.
    SetModel(String),
    Shutdown,
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
    /// Legacy one-stage acknowledgement retained for existing callers.
    ModelChanged(String),
    Compacted,
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
        let thread = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = event_tx.send(AgentEventEnvelope::agent(
                        AgentLane::Main,
                        AgentEvent::Error(error.to_string()),
                    ));
                    return;
                }
            };
            if let Err(error) = runtime.block_on(worker(config, command_rx, event_tx.clone())) {
                let _ = event_tx.send(AgentEventEnvelope::agent(
                    AgentLane::Main,
                    AgentEvent::Error(format!("{error:#}")),
                ));
            }
            let _ = event_tx.send(AgentEventEnvelope::agent(
                AgentLane::Main,
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
        Box::new(ControlledAgent::new(config.work_item_id))
    } else {
        Box::new(CopilotBridge::start(config))
    }
}

struct ControlledAgent {
    state: std::sync::Mutex<ControlledState>,
}

struct ControlledState {
    events: VecDeque<(Instant, AgentEventEnvelope)>,
    active_side: Option<String>,
    next_side: u64,
    busy_until: Instant,
}

impl ControlledAgent {
    fn new(work_item_id: String) -> Self {
        let mut events = VecDeque::new();
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
        Self {
            state: std::sync::Mutex::new(ControlledState {
                events,
                active_side: None,
                next_side: 1,
                busy_until: Instant::now(),
            }),
        }
    }

    fn schedule(&self, delay: Duration, event: AgentEventEnvelope) {
        let available_at = Instant::now() + delay;
        let mut state = self.state.lock().expect("controlled agent lock");
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
        let first = if context_draft {
            "Title: Controlled review\nWhat: Deterministic context\n".to_owned()
        } else {
            "Controlled ".to_owned()
        };
        let second = if context_draft {
            "Why: Repeatable tests\nHow: Fake streaming\nConsiderations: None\nOther approaches: Live agent".to_owned()
        } else {
            "streamed response.".to_owned()
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
            delay + Duration::from_secs(12),
            AgentEvent::ResponseDelta {
                outbound_id: id.clone(),
                delta: second,
            },
        );
        self.schedule_agent(
            lane,
            delay + Duration::from_secs(16),
            AgentEvent::ResponseComplete {
                outbound_id: id,
                aborted: false,
            },
        );
        let mut state = self.state.lock().expect("controlled agent lock");
        state.busy_until = state
            .busy_until
            .max(Instant::now() + delay + Duration::from_secs(16));
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
                let lane = self
                    .state
                    .lock()
                    .expect("controlled agent lock")
                    .active_side
                    .clone()
                    .map(|id| AgentLane::Side { id })
                    .unwrap_or(AgentLane::Main);
                self.schedule_activity(
                    lane,
                    Duration::ZERO,
                    None,
                    AgentActivity {
                        kind: ActivityKind::Intent,
                        label: "Steering the active response".into(),
                        tool: None,
                        detail: Some(outbound.text),
                    },
                );
            }
            AgentCommand::CancelQueued(outbound_id) => {
                let lane = {
                    let mut state = self.state.lock().expect("controlled agent lock");
                    let lane = state
                        .events
                        .iter()
                        .find(|(_, envelope)| {
                            envelope_outbound_id(envelope).as_deref() == Some(outbound_id.as_str())
                        })
                        .map(|(_, envelope)| envelope.lane.clone());
                    state.events.retain(|(_, envelope)| {
                        envelope_outbound_id(envelope).as_deref() != Some(outbound_id.as_str())
                    });
                    lane
                };
                if let Some(lane) = lane {
                    self.schedule_agent(
                        lane,
                        Duration::ZERO,
                        AgentEvent::QueueCancelled { outbound_id },
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
                        state.busy_until = Instant::now();
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
            AgentCommand::SetModel(model) => self.schedule_agent(
                AgentLane::Main,
                Duration::ZERO,
                AgentEvent::ModelChanged(model),
            ),
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
            return state.events.pop_front().map(|(_, event)| event);
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

fn envelope_outbound_id(envelope: &AgentEventEnvelope) -> Option<String> {
    match &envelope.event {
        LaneEvent::Agent(
            AgentEvent::Queued { outbound_id, .. }
            | AgentEvent::QueueCancelled { outbound_id }
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
    /// Identifier acknowledged by `session.send`. The current CLI exposes this
    /// as interaction metadata rather than as the `user.message` event ID, so
    /// the event itself must be observed before descendant events can be
    /// attributed safely.
    sdk_message_id: Option<String>,
    accepted_event_ids: HashSet<String>,
    message_buffers: HashMap<String, String>,
    message_order: Vec<String>,
    emitted_text: String,
    hidden_message_ids: HashSet<String>,
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
                detail: Some("pre-tool-use hook accepted the read-only operation".into()),
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
                kind: ActivityKind::Retry,
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
    id: String,
    parent_id: String,
    slot: SessionSlot,
    boundary_sent: bool,
}

async fn worker(
    config: BridgeConfig,
    mut commands: UnboundedReceiver<AgentCommand>,
    raw_events: Sender<AgentEventEnvelope>,
) -> Result<()> {
    let main_events = EventPublisher::new(raw_events, AgentLane::Main);
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
    let boot = sdk_call(
        "Copilot session create/resume",
        create_or_resume_session(&client, &config, &main_events),
    )
    .await?;
    let session = boot.session;
    let mut resumed_active = None;
    if boot.resumed {
        match sdk_call("Copilot history reload", session.get_events()).await {
            Ok(history) => {
                resumed_active = resumed_active_from_history(&history, session.id());
                main_events.emit(AgentEvent::HistoryLoaded(history_entries(&history)));
            }
            Err(error) => {
                main_events.activity(
                    None,
                    AgentActivity::other(format!("Could not restore timeline: {error}")),
                );
            }
        }
    }
    main_events.emit(AgentEvent::SessionReady {
        session_id: session.id().to_string(),
        resumed: boot.resumed,
        resume_warning: boot.resume_warning,
    });

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
                    AgentCommand::StartSide { outbound } => {
                        if side.is_some() {
                            main_events.activity(
                                None,
                                AgentActivity::other("A SIDE session is already active; exit it before starting another"),
                            );
                        } else {
                            let parent_id = main.session.id().to_string();
                            main_events.activity(
                                None,
                                AgentActivity::other("Creating ephemeral SIDE fork from MAIN…"),
                            );
                            match tokio::time::timeout(
                                SDK_CONTROL_TIMEOUT,
                                client.rpc().sessions().fork(SessionsForkRequest {
                                    session_id: SessionId::new(parent_id.clone()),
                                    to_event_id: None,
                                    name: None,
                                }),
                            )
                            .await
                            {
                                Ok(Ok(result)) => {
                                    let side_id = result.session_id.to_string();
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
                                            if let Some(outbound) = outbound {
                                                side_queue.push_front(outbound);
                                            }
                                            let boundary_sent =
                                                side_queue.front_mut().is_some_and(|outbound| {
                                                    apply_side_boundary(outbound);
                                                    true
                                                });
                                            side = Some(SideSession {
                                                id: side_id.clone(),
                                                parent_id: parent_id.clone(),
                                                slot: SessionSlot {
                                                    subscription: session.subscribe(),
                                                    session,
                                                },
                                                boundary_sent,
                                            });
                                            active_lane = lane;
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
                                            main_events.lifecycle(LaneEvent::SideFailed {
                                                message: format!(
                                                    "SIDE fork was created but could not be opened: {error}"
                                                ),
                                            })
                                        }
                                        Err(_) => {
                                            side_requested = false;
                                            side_queue.clear();
                                            main_events.lifecycle(LaneEvent::SideFailed {
                                                message: format!(
                                                    "SIDE fork {side_id} was created, but opening it exceeded {} seconds",
                                                    SDK_CONTROL_TIMEOUT.as_secs()
                                                ),
                                            })
                                        }
                                    }
                                }
                                Ok(Err(error)) => {
                                    side_requested = false;
                                    side_queue.clear();
                                    main_events.lifecycle(LaneEvent::SideFailed {
                                        message: format!("Could not create SIDE fork: {error}"),
                                    })
                                }
                                Err(_) => {
                                    side_requested = false;
                                    side_queue.clear();
                                    main_events.lifecycle(LaneEvent::SideFailed {
                                        message: format!(
                                            "SIDE creation exceeded {} seconds and was cancelled; MAIN is unchanged",
                                            SDK_CONTROL_TIMEOUT.as_secs()
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
                            disconnect_session(&side_session.slot.session).await.ok();
                            let cleanup_warning =
                                delete_session_with_timeout(&client, &side_id)
                                    .await
                                    .err()
                                    .map(|error| {
                                        format!(
                                            "SIDE closed, but its on-disk session could not be deleted: {error}"
                                        )
                                    });
                            side_events.lifecycle(LaneEvent::SideExited {
                                parent_id: side_session.parent_id,
                                side_id,
                                cleanup_warning,
                            });
                            side_queue.clear();
                            side_requested = false;
                            active_lane = AgentLane::Main;
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
                                Err(error) => events.activity(
                                    None,
                                    AgentActivity::other(format!(
                                        "Could not change model: {error}"
                                    )),
                                ),
                            },
                            Err(error) => events.activity(
                                None,
                                AgentActivity::other(format!("Could not change model: {error}")),
                            ),
                        }
                    }
                    AgentCommand::SetModel(model) => {
                        let selection = ModelSelection {
                            model_id: model,
                            reasoning_effort: None,
                            context_tier: None,
                        };
                        let events = main_events.on_lane(active_lane.clone());
                        let slot = if let Some(side) =
                            side.as_mut().filter(|_| active_lane != AgentLane::Main)
                        {
                            &mut side.slot
                        } else {
                            &mut main
                        };
                        match sdk_call(
                            "Copilot model selection",
                            slot.session
                                .set_model(&selection.model_id, Some(SetModelOptions::default())),
                        )
                        .await
                        {
                            Ok(()) => {
                                events.emit(AgentEvent::ModelSelectionChanged(selection.clone()));
                                events.emit(AgentEvent::ModelChanged(selection.model_id));
                            }
                            Err(error) => events.activity(
                                None,
                                AgentActivity::other(format!("Could not change model: {error}")),
                            ),
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
                        match sdk_call(
                            "Copilot MAIN fork",
                            client.rpc().sessions().fork(SessionsForkRequest {
                                session_id: SessionId::new(parent_id.clone()),
                                to_event_id: None,
                                name: None,
                            }),
                        )
                        .await
                        {
                            Ok(result) => {
                                let new_id = result.session_id.to_string();
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
                                    Err(error) => main_events.activity(
                                        None,
                                        AgentActivity::other(format!(
                                            "Fork was created but could not be activated: {error}"
                                        )),
                                    ),
                                }
                            }
                            Err(error) => main_events.activity(
                                None,
                                AgentActivity::other(format!("Could not fork session: {error}")),
                            ),
                        }
                    }
                    AgentCommand::Send(_)
                    | AgentCommand::Steer(_)
                    | AgentCommand::CancelQueued(_)
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
                        let position = main_queue.len() + usize::from(active.is_some());
                        let outbound_id = outbound.id.clone();
                        main_queue.push_back(outbound);
                        main_events.emit(AgentEvent::Queued { outbound_id, position });
                    }
                    AgentCommand::Steer(outbound) => {
                        let events = main_events.on_lane(active_lane.clone());
                        if active.is_none() {
                            let outbound_id = outbound.id.clone();
                            let queue = if active_lane == AgentLane::Main {
                                &mut main_queue
                            } else {
                                &mut side_queue
                            };
                            let position = queue.len();
                            queue.push_back(outbound);
                            events.emit(AgentEvent::Queued {
                                outbound_id,
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
                                Ok(_) => events.activity(
                                    outbound_id,
                                    AgentActivity {
                                        kind: ActivityKind::Intent,
                                        label: "Steering accepted by Copilot".into(),
                                        tool: None,
                                        detail: Some(outbound.text),
                                    },
                                ),
                                Err(error) => events.activity(
                                    outbound_id,
                                    AgentActivity::other(format!(
                                        "Steering could not interrupt the turn: {error}"
                                    )),
                                ),
                            }
                        }
                    }
                    AgentCommand::CancelQueued(outbound_id) => {
                        let before = main_queue.len() + side_queue.len();
                        main_queue.retain(|outbound| outbound.id != outbound_id);
                        side_queue.retain(|outbound| outbound.id != outbound_id);
                        let removed = main_queue.len() + side_queue.len() != before;
                        let events = main_events.on_lane(active_lane.clone());
                        if removed {
                            events.emit(AgentEvent::QueueCancelled { outbound_id });
                        } else {
                            events.activity(
                                active.as_ref().map(|turn| turn.outbound.id.clone()),
                                AgentActivity::other(
                                    "Prompt was already active or no longer queued",
                                ),
                            );
                        }
                    }
                    AgentCommand::SendSide(mut outbound) => {
                        if let Some(side_session) = side.as_mut() {
                            if !side_session.boundary_sent {
                                apply_side_boundary(&mut outbound);
                                side_session.boundary_sent = true;
                            }
                            let position = side_queue.len() + usize::from(active.is_some());
                            let outbound_id = outbound.id.clone();
                            side_queue.push_back(outbound);
                            main_events
                                .on_lane(AgentLane::Side {
                                    id: side_session.id.clone(),
                                })
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
                            main_events.on_lane(active_lane.clone()).activity(
                                None,
                                AgentActivity::other("Copilot is already idle"),
                            );
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
                    AgentCommand::Fork
                    | AgentCommand::Compact(_)
                    | AgentCommand::SelectModel(_)
                    | AgentCommand::SetModel(_) => {
                        let label = match &command {
                            AgentCommand::Fork => "Fork queued after the current response",
                            AgentCommand::Compact(_) => "Compaction queued after the current response",
                            AgentCommand::SelectModel(_) => {
                                "Model selection queued after the current response"
                            }
                            AgentCommand::SetModel(_) => "Model change queued after the current response",
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
            *active = Some(ActiveOutbound {
                outbound,
                response_started: false,
                turn_started: false,
                turn_id: None,
                sdk_message_id: Some(user_message_id),
                accepted_event_ids: HashSet::new(),
                message_buffers: HashMap::new(),
                message_order: Vec::new(),
                emitted_text: String::new(),
                hidden_message_ids: HashSet::new(),
            });
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

fn handle_session_event(
    event: SessionEvent,
    active: &mut Option<ActiveOutbound>,
    events: &impl EventOutput,
) {
    let root_agent_event = event.agent_id.is_none();
    let turn_or_session_event = event.event_type.starts_with("assistant.")
        || event.event_type.starts_with("tool.")
        || event.event_type.starts_with("skill.")
        || event.event_type.starts_with("subagent.")
        || matches!(event.event_type.as_str(), "session.idle" | "session.error");
    if let Some(active) = active.as_mut() {
        if !active.turn_started && event.event_type == "user.message" {
            let matches_sdk_id = active.sdk_message_id.as_deref().is_some_and(|id| {
                event.id == id
                    || event
                        .data
                        .get("interactionId")
                        .and_then(|value| value.as_str())
                        == Some(id)
            });
            let matches_content = event
                .data
                .get("content")
                .and_then(|value| value.as_str())
                == Some(active.outbound.text.as_str());
            if matches_sdk_id || matches_content {
                active.accepted_event_ids.insert(event.id.clone());
            }
        }
        let chained = active.accepted_event_ids.contains(&event.id)
            || event
                .parent_id
                .as_ref()
                .is_some_and(|parent| active.accepted_event_ids.contains(parent));
        if turn_or_session_event && !chained {
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
                label: "Thinking…".into(),
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
    if turn_scoped && active.as_ref().is_none_or(|active| !active.turn_started) {
        // A previous turn can leave buffered deltas behind after its idle
        // boundary. Never attach those bytes to a newly dispatched outbound
        // until that outbound's own turn-start event has arrived.
        return;
    }
    match event.event_type.as_str() {
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
                .and_then(|value| value.as_str())
                .unwrap_or("tool");
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
                    kind: ActivityKind::Other,
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
        "session.idle" => {
            if active.as_ref().is_some_and(|active| !active.turn_started) {
                events.activity(
                    active.as_ref().map(|turn| turn.outbound.id.clone()),
                    AgentActivity::other(
                        "Ignored an idle boundary from the previous turn; waiting for this turn to start",
                    ),
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
        _ => {}
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
        sdk_message_id: None,
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
                let session = client
                    .create_session(create_config(config, Some(events)))
                    .await?;
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
        session: client
            .create_session(create_config(config, Some(events)))
            .await?,
        resumed: false,
        resume_warning: None,
    })
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
    use std::collections::HashSet;
    use std::env;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use github_copilot_sdk::handler::PermissionHandler;
    use github_copilot_sdk::{
        PermissionRequestData, PermissionRequestKind, RequestId, SessionEvent, SessionId,
    };

    use super::{
        create_config, enqueue_message, handle_session_event, history_entries, model_option,
        resume_config, resumed_active_from_history, ActiveOutbound, ActivityKind, AgentCommand,
        AgentEvent, AgentLane, AgentRuntime, AgentSink, BridgeConfig, ControlledAgent,
        CopilotBridge, EventPublisher, HistoryEntry, LaneEvent, Outbound, OutboundKind,
        ReadOnlyPermissionHandler,
    };

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
            sdk_message_id: None,
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
        active_turn.sdk_message_id = Some("interaction-new".into());
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
            event("session.idle", serde_json::json!({"aborted": true})),
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
    #[ignore = "requires an authenticated Copilot CLI; see README.md"]
    fn live_copilot_streams_and_resumes_persisted_history() {
        assert_eq!(
            env::var("RQ_TUI_LIVE_COPILOT").as_deref(),
            Ok("1"),
            "set RQ_TUI_LIVE_COPILOT=1 to acknowledge this networked test"
        );
        let model = env::var("RQ_TUI_LIVE_MODEL").unwrap_or_else(|_| "gpt-5".into());
        let config = BridgeConfig {
            work_item_id: format!("live-test-{}", uuid::Uuid::new_v4()),
            session_root: env::current_dir().expect("current directory"),
            existing_session_id: None,
            model,
            reasoning_effort: None,
            context_tier: None,
            skill_directories: Vec::new(),
            plugin_directories: Vec::new(),
        };
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

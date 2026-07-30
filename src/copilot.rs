use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use github_copilot_sdk::handler::{PermissionHandler, PermissionResult};
use github_copilot_sdk::rpc::{HistoryCompactRequest, SessionsForkRequest};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::subscription::RecvErrorKind;
use github_copilot_sdk::{
    CliProgram, Client, ClientOptions, ResumeSessionConfig, SessionConfig, SessionEvent, SessionId,
    SystemMessageConfig,
};
use github_copilot_sdk::{PermissionRequestData, PermissionRequestKind, RequestId};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

const SIDE_BOUNDARY: &str = "Side conversation boundary.\n\
Everything before this boundary is inherited MAIN history and is reference context only, not the \
current task. Answer only the side question below. Do not continue plans or instructions from MAIN. \
This SIDE conversation is ephemeral and read-only; do not modify files or workspace state.";

fn apply_side_boundary(outbound: &mut Outbound) {
    outbound.text = format!("{SIDE_BOUNDARY}\n\nSide question:\n{}", outbound.text);
}

#[derive(Clone, Debug)]
pub(crate) struct BridgeConfig {
    pub(crate) work_item_id: String,
    pub(crate) session_root: PathBuf,
    pub(crate) existing_session_id: Option<String>,
    pub(crate) model: String,
    pub(crate) skill_directories: Vec<PathBuf>,
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

#[derive(Clone, Debug)]
pub(crate) enum AgentCommand {
    Send(Outbound),
    /// Fork the persisted main session and enter an ephemeral side lane.
    StartSide {
        outbound: Option<Outbound>,
    },
    /// Send a follow-up to the currently active side lane.
    SendSide(Outbound),
    /// Drop the current ephemeral side session and return to the main session.
    ExitSide,
    Abort,
    Fork,
    Compact(Option<String>),
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
    SideStarted { parent_id: String, side_id: String },
    SideExited { parent_id: String, side_id: String },
    SideFailed { message: String },
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
            AgentCommand::ExitSide => {
                let (side, delay) = {
                    let mut state = self.state.lock().expect("controlled agent lock");
                    let side = state.active_side.take();
                    let delay = state.busy_until.saturating_duration_since(Instant::now());
                    (side, delay)
                };
                if let Some(side_id) = side {
                    self.schedule(
                        delay,
                        AgentEventEnvelope {
                            lane: AgentLane::Side {
                                id: side_id.clone(),
                            },
                            event: LaneEvent::SideExited {
                                parent_id: "controlled-main".into(),
                                side_id,
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
    let client = Client::start(options).await?;
    let boot = create_or_resume_session(&client, &config).await?;
    let session = boot.session;
    if boot.resumed {
        match session.get_events().await {
            Ok(history) => {
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
    let mut active_lane = AgentLane::Main;
    let mut main_queue = VecDeque::new();
    let mut side_queue = VecDeque::new();
    let mut controls = VecDeque::new();
    let mut active: Option<ActiveOutbound> = None;

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
                            match client
                                .rpc()
                                .sessions()
                                .fork(SessionsForkRequest {
                                    session_id: SessionId::new(parent_id.clone()),
                                    to_event_id: None,
                                    name: None,
                                })
                                .await
                            {
                                Ok(result) => {
                                    let side_id = result.session_id.to_string();
                                    let mut resume =
                                        resume_config(SessionId::new(side_id.clone()), &config);
                                    resume.suppress_resume_event = Some(true);
                                    match client.resume_session(resume).await {
                                        Ok(session) => {
                                            let lane = AgentLane::Side { id: side_id.clone() };
                                            let side_events = main_events.on_lane(lane.clone());
                                            side = Some(SideSession {
                                                id: side_id.clone(),
                                                parent_id: parent_id.clone(),
                                                slot: SessionSlot {
                                                    subscription: session.subscribe(),
                                                    session,
                                                },
                                                boundary_sent: outbound.is_some(),
                                            });
                                            active_lane = lane;
                                            side_events.lifecycle(LaneEvent::SideStarted {
                                                parent_id,
                                                side_id: side_id.clone(),
                                            });
                                            side_events.activity(
                                                None,
                                                AgentActivity::other("SIDE ready; MAIN history is unchanged"),
                                            );
                                            if let Some(mut outbound) = outbound {
                                                apply_side_boundary(&mut outbound);
                                                let outbound_id = outbound.id.clone();
                                                side_queue.push_back(outbound);
                                                side_events.emit(AgentEvent::Queued {
                                                    outbound_id,
                                                    position: 0,
                                                });
                                            }
                                        }
                                        Err(error) => {
                                            main_events.lifecycle(LaneEvent::SideFailed {
                                                message: format!(
                                                    "SIDE fork was created but could not be opened: {error}"
                                                ),
                                            })
                                        }
                                    }
                                }
                                Err(error) => main_events.lifecycle(LaneEvent::SideFailed {
                                    message: format!("Could not create SIDE fork: {error}"),
                                }),
                            }
                        }
                    }
                    AgentCommand::ExitSide => {
                        if let Some(side_session) = side.take() {
                            let lane = AgentLane::Side {
                                id: side_session.id.clone(),
                            };
                            let side_events = main_events.on_lane(lane);
                            side_session.slot.session.disconnect().await.ok();
                            side_events.lifecycle(LaneEvent::SideExited {
                                parent_id: side_session.parent_id,
                                side_id: side_session.id,
                            });
                            side_queue.clear();
                            active_lane = AgentLane::Main;
                        } else {
                            main_events
                                .activity(None, AgentActivity::other("No SIDE session is active"));
                        }
                    }
                    AgentCommand::SetModel(model) => {
                        let events = main_events.on_lane(active_lane.clone());
                        let slot = if let Some(side) =
                            side.as_mut().filter(|_| active_lane != AgentLane::Main)
                        {
                            &mut side.slot
                        } else {
                            &mut main
                        };
                        match slot.session.set_model(&model, None).await {
                            Ok(()) => events.emit(AgentEvent::ModelChanged(model)),
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
                            Some(custom_instructions) => slot
                                .session
                                .rpc()
                                .history()
                                .compact_with_params(HistoryCompactRequest {
                                    custom_instructions: Some(custom_instructions),
                                })
                                .await
                                .map(|_| ()),
                            None => slot.session.rpc().history().compact().await.map(|_| ()),
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
                        match client
                            .rpc()
                            .sessions()
                            .fork(SessionsForkRequest {
                                session_id: SessionId::new(parent_id.clone()),
                                to_event_id: None,
                                name: None,
                            })
                            .await
                        {
                            Ok(result) => {
                                let new_id = result.session_id.to_string();
                                let mut resume =
                                    resume_config(SessionId::new(new_id.clone()), &config);
                                resume.suppress_resume_event = Some(true);
                                match client.resume_session(resume).await {
                                    Ok(session) => {
                                        main.session.disconnect().await.ok();
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
                    | AgentCommand::SendSide(_)
                    | AgentCommand::Abort
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
                    AgentCommand::Send(outbound) => {
                        let position = main_queue.len() + usize::from(active.is_some());
                        let outbound_id = outbound.id.clone();
                        main_queue.push_back(outbound);
                        main_events.emit(AgentEvent::Queued { outbound_id, position });
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
                        } else {
                            main_events.activity(None, AgentActivity::other("No SIDE session is active; use /side first"));
                        }
                    }
                    AgentCommand::Abort => {
                        if active.is_some() {
                            let slot = if let Some(side) = side.as_mut().filter(|_| active_lane != AgentLane::Main) {
                                &mut side.slot
                            } else {
                                &mut main
                            };
                            match slot.session.abort().await {
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
                    AgentCommand::Fork
                    | AgentCommand::Compact(_)
                    | AgentCommand::SetModel(_)
                    | AgentCommand::StartSide { .. }
                    | AgentCommand::ExitSide => {
                        let label = match &command {
                            AgentCommand::Fork => "Fork queued after the current response",
                            AgentCommand::Compact(_) => "Compaction queued after the current response",
                            AgentCommand::SetModel(_) => "Model change queued after the current response",
                            AgentCommand::StartSide { .. } => "SIDE creation queued after the current response",
                            AgentCommand::ExitSide => "Leaving SIDE queued after the current response",
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
                            slot.session.abort().await.ok();
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
                                main_events.on_lane(active_lane.clone()).activity(
                                    active.as_ref().map(|turn| turn.outbound.id.clone()),
                                    AgentActivity::other(format!(
                                        "Resynchronizing after {} skipped SDK event(s)",
                                        lagged.skipped()
                                    )),
                                );
                            }
                            RecvErrorKind::Closed => {
                                fail_active(
                                    &mut active,
                                    "Copilot event stream closed".into(),
                                    &main_events.on_lane(active_lane.clone()),
                                );
                                main_events.on_lane(active_lane.clone()).emit(AgentEvent::Error(error.to_string()));
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
        side.slot.session.disconnect().await.ok();
    }
    main.session.disconnect().await.ok();
    client.stop().await.ok();
    Ok(())
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
    match session.send(outbound.text.clone()).await {
        Ok(_) => {
            *active = Some(ActiveOutbound {
                outbound,
                response_started: false,
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
    match event.event_type.as_str() {
        "assistant.turn_start" if root_agent_event => {
            send_activity(
                active,
                AgentActivity {
                    kind: ActivityKind::Intent,
                    label: "Thinking…".into(),
                    tool: None,
                    detail: None,
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
            if let Some(active) = active.take() {
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

async fn create_or_resume_session(client: &Client, config: &BridgeConfig) -> Result<SessionBoot> {
    if let Some(id) = &config.existing_session_id {
        match client
            .resume_session(resume_config(SessionId::new(id.clone()), config))
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
                let session = client.create_session(create_config(config)).await?;
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
        session: client.create_session(create_config(config)).await?,
        resumed: false,
        resume_warning: None,
    })
}

fn create_config(config: &BridgeConfig) -> SessionConfig {
    SessionConfig::default()
        .with_model(config.model.clone())
        .with_streaming(true)
        .with_working_directory(config.session_root.clone())
        .with_skill_directories(config.skill_directories.clone())
        .with_system_message(system_message(&config.work_item_id))
        .with_permission_handler(Arc::new(ReadOnlyPermissionHandler))
}

fn resume_config(id: SessionId, config: &BridgeConfig) -> ResumeSessionConfig {
    ResumeSessionConfig::new(id)
        .with_model(config.model.clone())
        .with_streaming(true)
        .with_working_directory(config.session_root.clone())
        .with_skill_directories(config.skill_directories.clone())
        .with_system_message(system_message(&config.work_item_id))
        .with_permission_handler(Arc::new(ReadOnlyPermissionHandler))
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
    use std::env;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use github_copilot_sdk::handler::PermissionHandler;
    use github_copilot_sdk::{
        PermissionRequestData, PermissionRequestKind, RequestId, SessionEvent, SessionId,
    };

    use super::{
        handle_session_event, history_entries, ActiveOutbound, ActivityKind, AgentCommand,
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
            parent_id: None,
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
            skill_directories: Vec::new(),
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

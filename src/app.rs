use std::cmp;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::annotations::anchor_from_diff;
use crate::chat_selection::{
    ChatCursor, ChatLayout, ChatMessageId, ChatPoint, ChatSelection, ChatSelectionMode, CopyPolicy,
    Movement,
};
use crate::copilot::{ModelOption, ModelSelection, PruneSessionOutcome};
use crate::diff::{DiffFile, DiffSet, LineKind};
use crate::domain::{
    AnchorSide, Annotation, AnnotationKind, AskMessage, DeliveryState, PendingChat, Placement,
    Version,
};
use crate::review_stream::{
    InlineAnnotation, ReviewFile, ReviewRow, ReviewStream, SourceSide, StreamMovement,
};
use crate::terminal_text::{
    cell_width, floor_grapheme_boundary, grapheme_indices, next_grapheme_boundary,
    previous_grapheme_boundary,
};
use crate::work_item::ResolvedWorkItem;

pub(crate) const INLINE_COMPOSER_ID: &str = "zzzzzzzz-inline-composer";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Screen {
    #[default]
    Review,
    Chat,
    Settings,
    Versions,
    ContextEditor,
    Prune,
    Recovery,
    AgentStatus,
    Queue,
    ModelPicker,
    Preview,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum InputMode {
    #[default]
    Normal,
    Visual,
    Command,
    Search,
    Compose,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Focus {
    FilePicker,
    #[default]
    Diff,
    Chat,
    InlineAsk,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DiffLayout {
    Split,
    #[default]
    Unified,
}

impl DiffLayout {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Unified => "unified",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MarkdownPreview {
    #[default]
    Inline,
    Browser,
}

impl MarkdownPreview {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Browser => "browser",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ModelPickerStage {
    #[default]
    Model,
    Reasoning,
    Context,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Quit {
        force: bool,
    },
    Snapshot,
    Sync,
    ExpandContext {
        all: bool,
    },
    Export {
        format: Option<String>,
    },
    GenerateContext,
    AttachContext(String),
    CreateAnnotation {
        kind: AnnotationKind,
        text: String,
        selection: DiffSelection,
    },
    FollowUpAsk {
        annotation_id: String,
        text: String,
    },
    EditAnnotation {
        annotation_id: String,
        text: String,
    },
    EditAskMessage {
        annotation_id: String,
        message_id: String,
        text: String,
    },
    RepinAnnotation {
        annotation_id: String,
        selection: DiffSelection,
    },
    DeleteAnnotation(String),
    UndoAnnotation,
    OpenVersion {
        repo_id: String,
        version_id: String,
    },
    LoadPrune,
    PruneWorkItems {
        ids: Vec<String>,
        export_first: bool,
    },
    SendChat(String),
    SteerChat(String),
    CancelQueued(String),
    ReplaceQueued {
        outbound_id: String,
        text: String,
    },
    StartSide(Option<String>),
    ExitSide,
    AbortAgent,
    ResendPendingAsk(AskMessage),
    DiscardPendingAsk(String),
    ResendPendingChat(PendingChat),
    DiscardPendingChat(String),
    ResendPendingComments,
    DiscardPendingComments,
    ResendPendingContext,
    DiscardPendingContext,
    Fork,
    Compact(Option<String>),
    LoadModels,
    SelectModel(ModelSelection),
    SetModel(String),
    SetBase {
        branch: String,
        repo: Option<String>,
    },
    SetExpandStep(usize),
    SetDefaultDiffLayout(DiffLayout),
    SetFileTreeDefault(bool),
    SetMarkdownPreview(MarkdownPreview),
    Preview,
    PreviewBrowser,
    Yank(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPrune {
    pub(crate) request_id: String,
    pub(crate) work_item_ids: Vec<String>,
    pub(crate) skipped_current: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadyPrune {
    pub(crate) request_id: String,
    pub(crate) outcomes: Vec<PruneSessionOutcome>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiffSelection {
    pub(crate) start_row: usize,
    pub(crate) end_row: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewSelectionMode {
    Character,
    Line,
    Block,
}

impl ReviewSelectionMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Character => "CHAR",
            Self::Line => "LINE",
            Self::Block => "BLOCK",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewRowSelection {
    Whole,
    Columns { start: usize, end: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ComposeTarget {
    Annotation(AnnotationKind),
    FollowUp(String),
    EditAnnotation(String),
    EditAskMessage {
        annotation_id: String,
        message_id: String,
    },
    EditQueued(String),
    Chat,
    Context,
    SettingBase,
    SettingExpandStep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatEntry {
    /// Immutable identity used by semantic transcript selection.  It is never
    /// derived from a viewport position, outbound ID, or the mutable text.
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) text: String,
    pub(crate) streaming: bool,
    pub(crate) annotation_id: Option<String>,
    pub(crate) outbound_id: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug)]
struct PreviewReturnState {
    screen: Screen,
    focus: Focus,
    input_mode: InputMode,
    compose: String,
    compose_cursor: usize,
    compose_scroll: usize,
    compose_target: Option<ComposeTarget>,
    status: String,
}

/// The semantic and viewport state belonging to MAIN while SIDE owns the
/// visible chat vector.  The renderer can rebuild width-specific layout rows
/// after the swap; these values are the user-facing navigation contract.
#[derive(Clone, Debug)]
struct ChatViewportState {
    focus: Focus,
    input_mode: InputMode,
    input_return_mode: InputMode,
    compose: String,
    compose_cursor: usize,
    compose_scroll: usize,
    compose_target: Option<ComposeTarget>,
    chat_cursor: usize,
    chat_scroll: usize,
    chat_total_rows: usize,
    chat_viewport_rows: usize,
    chat_autofollow: bool,
    chat_navigation: Option<ChatCursor>,
    chat_selection: Option<ChatSelection>,
    chat_display_rows: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AgentPhase {
    Connecting,
    Queued,
    Planning,
    Tool,
    Responding,
    Stopping,
    #[default]
    Idle,
    Failed,
    Disconnected,
}

impl AgentPhase {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Connecting => "CONNECTING",
            Self::Queued => "QUEUED",
            Self::Planning => "THINKING",
            Self::Tool => "TOOL",
            Self::Responding => "RESPONDING",
            Self::Stopping => "STOPPING",
            Self::Idle => "IDLE",
            Self::Failed => "FAILED",
            Self::Disconnected => "OFFLINE",
        }
    }

    pub(crate) fn is_active(self) -> bool {
        matches!(
            self,
            Self::Connecting
                | Self::Queued
                | Self::Planning
                | Self::Tool
                | Self::Responding
                | Self::Stopping
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AgentTimelineEntry {
    pub(crate) at: Instant,
    pub(crate) label: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentProgress {
    pub(crate) phase: AgentPhase,
    pub(crate) summary: String,
    pub(crate) detail: String,
    pub(crate) active_outbound_id: Option<String>,
    pub(crate) turn_started_at: Option<Instant>,
    pub(crate) last_event_at: Instant,
    pub(crate) event_count: usize,
    pub(crate) queue_depth: usize,
    pub(crate) timeline: VecDeque<AgentTimelineEntry>,
    display_now: Option<Instant>,
}

impl Default for AgentProgress {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            phase: AgentPhase::Connecting,
            summary: "Starting Copilot SDK session".into(),
            detail: "Waiting for the SDK session-ready event".into(),
            active_outbound_id: None,
            turn_started_at: Some(now),
            last_event_at: now,
            event_count: 0,
            queue_depth: 0,
            timeline: VecDeque::from([AgentTimelineEntry {
                at: now,
                label: "Starting Copilot SDK session".into(),
            }]),
            display_now: None,
        }
    }
}

impl AgentProgress {
    pub(crate) fn record(
        &mut self,
        phase: AgentPhase,
        summary: impl Into<String>,
        detail: impl Into<String>,
        outbound_id: Option<String>,
    ) {
        let now = Instant::now();
        let summary = summary.into();
        let detail = detail.into();
        if phase.is_active() && !self.phase.is_active() {
            self.turn_started_at = Some(now);
        }
        if !phase.is_active() {
            self.turn_started_at = None;
        }
        self.phase = phase;
        self.summary = summary;
        self.detail = detail;
        self.active_outbound_id = outbound_id;
        self.last_event_at = now;
        self.event_count = self.event_count.saturating_add(1);
        self.timeline.push_back(AgentTimelineEntry {
            at: now,
            label: if self.detail.is_empty() || self.detail == self.summary {
                self.summary.clone()
            } else {
                format!("{} — {}", self.summary, self.detail)
            },
        });
        while self.timeline.len() > 24 {
            self.timeline.pop_front();
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.turn_started_at
            .map(|started| self.now().saturating_duration_since(started))
            .unwrap_or_default()
    }

    pub(crate) fn record_queued(&mut self, summary: impl Into<String>, detail: impl Into<String>) {
        let now = Instant::now();
        let summary = summary.into();
        let detail = detail.into();
        if self.active_outbound_id.is_none() {
            self.phase = AgentPhase::Queued;
            self.summary = summary.clone();
            self.detail = detail.clone();
            self.turn_started_at.get_or_insert(now);
        }
        self.last_event_at = now;
        self.event_count = self.event_count.saturating_add(1);
        self.timeline.push_back(AgentTimelineEntry {
            at: now,
            label: if detail.is_empty() || detail == summary {
                summary
            } else {
                format!("{summary} — {detail}")
            },
        });
        while self.timeline.len() > 24 {
            self.timeline.pop_front();
        }
    }

    pub(crate) fn record_observation(
        &mut self,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let now = Instant::now();
        let summary = summary.into();
        let detail = detail.into();
        self.last_event_at = now;
        self.event_count = self.event_count.saturating_add(1);
        self.timeline.push_back(AgentTimelineEntry {
            at: now,
            label: if detail.is_empty() || detail == summary {
                summary
            } else {
                format!("{summary} — {detail}")
            },
        });
        while self.timeline.len() > 24 {
            self.timeline.pop_front();
        }
    }

    pub(crate) fn last_event_age(&self) -> Duration {
        self.now().saturating_duration_since(self.last_event_at)
    }

    pub(crate) fn timeline_age(&self, at: Instant) -> Duration {
        self.now().saturating_duration_since(at)
    }

    pub(crate) fn freeze_display_clock(&mut self, now: Instant) {
        self.display_now = Some(now);
    }

    fn now(&self) -> Instant {
        self.display_now.unwrap_or_else(Instant::now)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VersionChoice {
    pub(crate) repo_name: String,
    pub(crate) version: Version,
    pub(crate) asks: usize,
    pub(crate) comments: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct PruneChoice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) last_opened_at: String,
    pub(crate) versions: usize,
    pub(crate) annotations: usize,
    pub(crate) selected: bool,
}

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    (
        "agent-status",
        "inspect Copilot liveness and recent SDK events",
    ),
    ("queue", "inspect active and queued Copilot questions"),
    (
        "steer <correction>",
        "redirect the active response immediately",
    ),
    ("side [question]", "start an ephemeral side conversation"),
    ("main", "leave the side conversation and return to main"),
    ("stop", "cancel the active Copilot response"),
    ("abort", "cancel the active Copilot response"),
    ("progress", "inspect Copilot liveness and recent SDK events"),
    ("side-exit", "leave the side conversation and return to main"),
    ("diff split", "show the side-by-side diff"),
    ("diff unified", "show a single-column diff"),
    ("diff expand", "expand all folded context"),
    ("model", "pick a runtime-supported model in stages"),
    ("fork", "fork and activate a persistent session"),
    (
        "compact [instructions]",
        "compact persistent session history",
    ),
    ("versions", "open exact review-version history"),
    ("snapshot", "pin the current working-tree version"),
    ("generate-context", "draft structured review context"),
    (
        "preview-browser",
        "open Markdown preview in the system browser",
    ),
    ("export [markdown|json]", "export review annotations"),
    ("prune", "remove old work items"),
    ("settings", "open settings"),
    ("sync", "refresh repositories and remote PRs"),
    (
        "base <branch> [--repo <name>]",
        "change a repository base branch",
    ),
    ("q", "quit when no delivery is pending"),
    ("q!", "force quit"),
    ("quit", "quit when no delivery is pending"),
    ("quit!", "force quit"),
];

pub(crate) fn command_matches(prefix: &str) -> Vec<(&'static str, &'static str)> {
    let typed = prefix.split_whitespace().collect::<Vec<_>>();
    COMMANDS
        .iter()
        .copied()
        .filter(|(command, _)| command_matches_tokens(&typed, command))
        .collect()
}

fn command_matches_tokens(typed: &[&str], candidate: &str) -> bool {
    if typed.is_empty() {
        return true;
    }
    let candidate_tokens = candidate.split_whitespace().collect::<Vec<_>>();
    typed.iter().enumerate().all(|(index, token)| {
        let Some(candidate_token) = candidate_tokens.get(index) else {
            return false;
        };
        is_command_argument(candidate_token) || candidate_token.starts_with(token)
    })
}

fn is_command_argument(token: &str) -> bool {
    token.contains('<') || token.contains('[') || token.contains(']')
}

fn literal_command_completion(typed: &str, candidate: &str) -> bool {
    let typed_tokens = typed.split_whitespace().collect::<Vec<_>>();
    let candidate_tokens = candidate.split_whitespace().collect::<Vec<_>>();
    !typed_tokens.is_empty()
        && typed_tokens.len() <= candidate_tokens.len()
        && candidate_tokens.iter().all(|token| !is_command_argument(token))
        && typed_tokens
            .iter()
            .enumerate()
            .all(|(index, token)| candidate_tokens[index].starts_with(token))
}

fn command_seed(command: &str) -> String {
    command
        .split_whitespace()
        .take_while(|part| !part.starts_with('<') && !part.starts_with('['))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug)]
pub(crate) struct AppState {
    pub(crate) work_item: ResolvedWorkItem,
    pub(crate) screen: Screen,
    pub(crate) previous_screen: Screen,
    pub(crate) input_mode: InputMode,
    pub(crate) focus: Focus,
    pub(crate) layout: DiffLayout,
    pub(crate) default_layout: DiffLayout,
    pub(crate) picker_open: bool,
    pub(crate) file_tree_default_open: bool,
    pub(crate) markdown_preview: MarkdownPreview,
    pub(crate) cache_directory: String,
    pub(crate) storage_path: String,
    pub(crate) skill_directories: String,
    pub(crate) repo_index: usize,
    pub(crate) file_index: usize,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// Cursor and viewport in the one continuous semantic Review stream.
    /// `cursor` remains the source-row index used for annotations/visual mode.
    pub(crate) review_cursor: usize,
    pub(crate) review_scroll: usize,
    pub(crate) visual_anchor: Option<usize>,
    pub(crate) review_visual_mode: Option<ReviewSelectionMode>,
    pub(crate) review_visual_anchor_column: usize,
    pub(crate) review_visual_column: usize,
    pub(crate) review_visual_preferred_column: usize,
    pub(crate) review_horizontal_scroll: usize,
    pub(crate) review_content_width: usize,
    /// Width-specific renderer index. Cursor and selection endpoints remain
    /// source based, therefore survive a layout rebuild.
    pub(crate) chat_layout: Option<ChatLayout>,
    pub(crate) chat_navigation: Option<ChatCursor>,
    pub(crate) chat_selection: Option<ChatSelection>,
    pub(crate) chat_display_rows: Vec<usize>,
    pub(crate) command: String,
    pub(crate) command_index: usize,
    pub(crate) command_scroll: usize,
    pub(crate) command_viewport_rows: usize,
    pub(crate) search: String,
    pub(crate) compose: String,
    pub(crate) compose_cursor: usize,
    pub(crate) compose_scroll: usize,
    /// Inner terminal-cell width used by visual-row cursor movement.
    pub(crate) compose_wrap_width: usize,
    pub(crate) compose_target: Option<ComposeTarget>,
    pub(crate) input_return_mode: InputMode,
    pub(crate) status: String,
    pub(crate) quit_guard: Option<String>,
    pub(crate) annotations: Vec<(Annotation, Placement)>,
    pub(crate) ask_threads: HashMap<String, Vec<AskMessage>>,
    pub(crate) collapsed_annotations: HashSet<String>,
    pub(crate) collapsed_repos: HashSet<String>,
    pub(crate) expanded_context: HashMap<(String, String), usize>,
    pub(crate) expand_step: usize,
    pub(crate) chat: Vec<ChatEntry>,
    pub(crate) main_chat: Option<Vec<ChatEntry>>,
    main_chat_viewport: Option<ChatViewportState>,
    pub(crate) pending_side_entries: Vec<ChatEntry>,
    pub(crate) chat_cursor: usize,
    pub(crate) chat_scroll: usize,
    pub(crate) chat_total_rows: usize,
    pub(crate) chat_viewport_rows: usize,
    pub(crate) chat_autofollow: bool,
    pub(crate) preview_markdown: Option<String>,
    pub(crate) preview_scroll: usize,
    pub(crate) preview_total_rows: usize,
    preview_return: Option<PreviewReturnState>,
    pub(crate) context_draft: String,
    pub(crate) context_streaming: bool,
    pub(crate) agent_connected: bool,
    pub(crate) agent_activity: String,
    pub(crate) agent_progress: AgentProgress,
    pub(crate) side_starting: bool,
    pub(crate) side_active: bool,
    pub(crate) side_session_id: Option<String>,
    pub(crate) last_usage: Option<String>,
    pub(crate) pending_outbound_ids: HashSet<String>,
    pub(crate) side_outbound_ids: HashSet<String>,
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) context_tier: Option<String>,
    pub(crate) model_options: Vec<ModelOption>,
    pub(crate) model_picker_stage: ModelPickerStage,
    pub(crate) model_picker_index: usize,
    pub(crate) pending_model_selection: Option<ModelSelection>,
    pub(crate) pending_prefix: String,
    pub(crate) pending_prefix_started: Option<Instant>,
    pub(crate) pending_asks: Vec<AskMessage>,
    pub(crate) pending_chats: Vec<PendingChat>,
    pub(crate) pending_comment_ids: Vec<String>,
    pub(crate) pending_context: bool,
    pub(crate) recovery_index: usize,
    pub(crate) deleted_annotation: Option<(Annotation, Vec<Placement>, Vec<AskMessage>)>,
    pub(crate) versions: Vec<VersionChoice>,
    pub(crate) version_index: usize,
    pub(crate) prune_items: Vec<PruneChoice>,
    pub(crate) prune_index: usize,
    pub(crate) pending_prune: Option<PendingPrune>,
    pub(crate) ready_prune: Option<ReadyPrune>,
    pub(crate) prune_recoveries: HashSet<String>,
    pub(crate) settings_index: usize,
    pub(crate) should_quit: bool,
    pub(crate) viewport_height: usize,
}

impl AppState {
    const CTRL_W_TIMEOUT: Duration = Duration::from_millis(1_500);

    pub(crate) fn new(work_item: ResolvedWorkItem) -> Self {
        let mut state = Self {
            work_item,
            screen: Screen::Review,
            previous_screen: Screen::Review,
            input_mode: InputMode::Normal,
            focus: Focus::Diff,
            layout: DiffLayout::Unified,
            default_layout: DiffLayout::Unified,
            picker_open: false,
            file_tree_default_open: true,
            markdown_preview: MarkdownPreview::Inline,
            cache_directory: "runtime default".into(),
            storage_path: "runtime default".into(),
            skill_directories: "runtime default".into(),
            repo_index: 0,
            file_index: 0,
            cursor: 0,
            scroll: 0,
            review_cursor: 0,
            review_scroll: 0,
            visual_anchor: None,
            review_visual_mode: None,
            review_visual_anchor_column: 0,
            review_visual_column: 0,
            review_visual_preferred_column: 0,
            review_horizontal_scroll: 0,
            review_content_width: 1,
            chat_layout: None,
            chat_navigation: None,
            chat_selection: None,
            chat_display_rows: Vec::new(),
            command: String::new(),
            command_index: 0,
            command_scroll: 0,
            command_viewport_rows: 7,
            search: String::new(),
            compose: String::new(),
            compose_cursor: 0,
            compose_scroll: 0,
            compose_wrap_width: 80,
            compose_target: None,
            input_return_mode: InputMode::Normal,
            status: String::new(),
            quit_guard: None,
            annotations: Vec::new(),
            ask_threads: HashMap::new(),
            collapsed_annotations: HashSet::new(),
            collapsed_repos: HashSet::new(),
            expanded_context: HashMap::new(),
            expand_step: 10,
            chat: Vec::new(),
            main_chat: None,
            main_chat_viewport: None,
            pending_side_entries: Vec::new(),
            chat_cursor: 0,
            chat_scroll: 0,
            chat_total_rows: 0,
            chat_viewport_rows: 0,
            chat_autofollow: true,
            preview_markdown: None,
            preview_scroll: 0,
            preview_total_rows: 0,
            preview_return: None,
            context_draft: String::new(),
            context_streaming: false,
            agent_connected: false,
            agent_activity: "Connecting…".into(),
            agent_progress: AgentProgress::default(),
            side_starting: false,
            side_active: false,
            side_session_id: None,
            last_usage: None,
            pending_outbound_ids: HashSet::new(),
            side_outbound_ids: HashSet::new(),
            model: "gpt-5".into(),
            reasoning_effort: None,
            context_tier: None,
            model_options: Vec::new(),
            model_picker_stage: ModelPickerStage::Model,
            model_picker_index: 0,
            pending_model_selection: None,
            pending_prefix: String::new(),
            pending_prefix_started: None,
            pending_asks: Vec::new(),
            pending_chats: Vec::new(),
            pending_comment_ids: Vec::new(),
            pending_context: false,
            recovery_index: 0,
            deleted_annotation: None,
            versions: Vec::new(),
            version_index: 0,
            prune_items: Vec::new(),
            prune_index: 0,
            pending_prune: None,
            ready_prune: None,
            prune_recoveries: HashSet::new(),
            settings_index: 0,
            should_quit: false,
            viewport_height: 20,
        };
        state.sync_review_cursor_to_current_file();
        state
    }

    pub(crate) fn current_diff(&self) -> Option<&DiffSet> {
        self.work_item
            .repos
            .get(self.repo_index)
            .map(|repo| &repo.diff)
    }

    pub(crate) fn tick(&mut self, now: Instant) {
        if self.quit_guard.is_some() && !self.has_unsubmitted_work() && self.compose.is_empty() {
            self.quit_guard = None;
        }
        if !self.pending_prefix.is_empty()
            && self.pending_prefix_started.is_some_and(|started| {
                now.saturating_duration_since(started) >= Self::CTRL_W_TIMEOUT
            })
        {
            let prefix = self.pending_prefix.clone();
            self.pending_prefix.clear();
            self.pending_prefix_started = None;
            self.status = match prefix.as_str() {
                "preview-g" => "Markdown preview prefix timed out · scroll unchanged".into(),
                "ctrl-w" => "CTRL-W focus navigation timed out · focus unchanged".into(),
                _ => format!("KEY {prefix} timed out · no command was run"),
            };
        }
        // Queue depth is derived from the lane-visible transcript rather than
        // being trusted to survive a SIDE swap's teardown bookkeeping.
        self.agent_progress.queue_depth = self.queue_entry_ids().len();
    }

    pub(crate) fn current_file(&self) -> Option<&DiffFile> {
        self.current_diff()
            .and_then(|diff| diff.files.get(self.file_index))
    }

    pub(crate) fn current_line_count(&self) -> usize {
        self.current_file()
            .map(|file| file.visible_lines().count())
            .unwrap_or(0)
    }

    pub(crate) fn review_stream(&self) -> ReviewStream {
        let files = self
            .work_item
            .repos
            .iter()
            .flat_map(|repo| {
                repo.diff.files.iter().cloned().map(|file| {
                    ReviewFile::new(repo.record.id.clone(), repo.record.name.clone(), file)
                })
            })
            .collect::<Vec<_>>();
        let mut annotations = self
            .annotations
            .iter()
            .map(|(annotation, placement)| {
                let marker = if placement.outdated {
                    "!"
                } else if placement.ambiguous {
                    "≈"
                } else {
                    ""
                };
                let kind = match annotation.kind {
                    AnnotationKind::Ask => "Ask",
                    AnnotationKind::Comment => "Comment",
                };
                let side = match placement.side {
                    AnchorSide::Old => ("L", SourceSide::Old),
                    AnchorSide::New => ("R", SourceSide::New),
                };
                let body = match annotation.kind {
                    AnnotationKind::Comment => {
                        format!("❯ {}", annotation.text.as_deref().unwrap_or_default())
                    }
                    AnnotationKind::Ask => self
                        .ask_threads
                        .get(&annotation.id)
                        .map(|thread| {
                            thread
                                .iter()
                                .map(|message| {
                                    let speaker = if message.role == "assistant" {
                                        "🤖"
                                    } else {
                                        "❯"
                                    };
                                    let waiting =
                                        if message.delivery_state == DeliveryState::Pending {
                                            " · queued"
                                        } else {
                                            ""
                                        };
                                    format!("{speaker} {}{waiting}", message.text)
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .filter(|thread| !thread.is_empty())
                        .unwrap_or_else(|| "⏺ Ask queued · waiting for Copilot".into()),
                };
                let mut inline = InlineAnnotation::new(
                    annotation.id.clone(),
                    annotation.file_path.clone(),
                    side.1,
                    placement.line_start.max(0) as usize,
                    placement.line_end.max(placement.line_start).max(0) as usize,
                    format!(
                        "{marker}{kind} · {} {}{}",
                        annotation.file_path.display(),
                        side.0,
                        placement.line_start
                    ),
                    body,
                )
                .in_repo(annotation.repo_id.clone());
                inline.collapsed = self.collapsed_annotations.contains(&annotation.id);
                inline
            })
            .collect::<Vec<_>>();
        if let Some(composer) = self.inline_composer_annotation() {
            annotations.push(composer);
        }
        ReviewStream::for_files(&files, &annotations)
    }

    fn inline_composer_annotation(&self) -> Option<InlineAnnotation> {
        let target = self.compose_target.as_ref()?;
        let (repo_id, file_path, side, line_start, line_end, title) = match target {
            ComposeTarget::Annotation(kind) => {
                let repo = self.work_item.repos.get(self.repo_index)?;
                let file = self.current_file()?;
                let selection = self.diff_selection();
                let anchor = anchor_from_diff(file, selection.start_row, selection.end_row).ok()?;
                (
                    repo.record.id.clone(),
                    file.display_path.clone(),
                    match anchor.side {
                        AnchorSide::Old => SourceSide::Old,
                        AnchorSide::New => SourceSide::New,
                    },
                    anchor.line_start,
                    anchor.line_end,
                    match kind {
                        AnnotationKind::Ask => "Ask · draft",
                        AnnotationKind::Comment => "Comment · draft",
                    }
                    .to_owned(),
                )
            }
            ComposeTarget::FollowUp(id) => {
                let (annotation, placement) = self
                    .annotations
                    .iter()
                    .find(|(annotation, _)| &annotation.id == id)?;
                (
                    annotation.repo_id.clone(),
                    annotation.file_path.clone(),
                    match placement.side {
                        AnchorSide::Old => SourceSide::Old,
                        AnchorSide::New => SourceSide::New,
                    },
                    placement.line_start.max(0) as usize,
                    placement.line_end.max(placement.line_start).max(0) as usize,
                    "Ask follow-up · draft".into(),
                )
            }
            ComposeTarget::EditAnnotation(id)
            | ComposeTarget::EditAskMessage {
                annotation_id: id, ..
            } => {
                let (annotation, placement) = self
                    .annotations
                    .iter()
                    .find(|(annotation, _)| &annotation.id == id)?;
                (
                    annotation.repo_id.clone(),
                    annotation.file_path.clone(),
                    match placement.side {
                        AnchorSide::Old => SourceSide::Old,
                        AnchorSide::New => SourceSide::New,
                    },
                    placement.line_start.max(0) as usize,
                    placement.line_end.max(placement.line_start).max(0) as usize,
                    "Edit inline annotation".into(),
                )
            }
            ComposeTarget::EditQueued(_)
            | ComposeTarget::Chat
            | ComposeTarget::Context
            | ComposeTarget::SettingBase
            | ComposeTarget::SettingExpandStep => return None,
        };
        let mut draft = self.compose.clone();
        if self.input_mode == InputMode::Compose {
            let cursor = floor_grapheme_boundary(&draft, self.compose_cursor);
            draft.insert(cursor, '▏');
        }
        Some(
            InlineAnnotation::new(
                INLINE_COMPOSER_ID,
                file_path,
                side,
                line_start,
                line_end,
                format!(
                    "{title} · {} lines {line_start}-{line_end}",
                    match side {
                        SourceSide::Old => "old",
                        SourceSide::New => "new",
                    }
                ),
                format!("↳ Enter submit · Esc keep · Ctrl-C discard · ↑/↓ scroll\n❯ {draft}"),
            )
            .in_repo(repo_id),
        )
    }

    fn focus_inline_composer(&mut self) {
        self.review_cursor = self
            .review_stream()
            .rows()
            .iter()
            .rposition(|row| {
                matches!(
                    row,
                    ReviewRow::Annotation { block, .. }
                        if block.annotation_id == INLINE_COMPOSER_ID
                            && matches!(
                                block.part,
                                crate::review_stream::AnnotationRowPart::Body { .. }
                            )
                )
            })
            .unwrap_or(self.review_cursor);
    }

    fn move_review_stream(&mut self, movement: StreamMovement) {
        let origin = (self.repo_index, self.file_index);
        let was_visual = self.input_mode == InputMode::Visual;
        let mut stream = self.review_stream();
        stream.set_cursor(self.review_cursor);
        stream.move_by(movement, self.viewport_height.max(1));
        self.review_cursor = stream.cursor();
        let row = stream.current().cloned();
        if let Some(row) = row.as_ref() {
            self.sync_source_from_review_row(row);
        }
        if was_visual && origin != (self.repo_index, self.file_index) {
            self.clear_visual_selection();
            self.status = "Visual selection cleared at the file boundary".into();
        }
    }

    fn sync_source_from_review_row(&mut self, row: &ReviewRow) {
        if let Some(anchor) = row.source_anchor() {
            self.set_current_review_location(&anchor.repo_id, &anchor.file, anchor.visible_line);
            return;
        }
        match row {
            ReviewRow::FileHeader { repo_id, path, .. } => {
                self.set_current_review_location(repo_id, path, 0)
            }
            ReviewRow::HunkHeader {
                repo_id,
                file,
                hunk,
                ..
            } => {
                let visible =
                    self.work_item
                        .repos
                        .iter()
                        .find(|repo| repo.record.id == *repo_id)
                        .and_then(|repo| {
                            repo.diff.files.iter().find(|candidate| {
                                candidate.path().to_string_lossy() == file.as_str()
                            })
                        })
                        .map(|file| {
                            file.hunks
                                .iter()
                                .take(*hunk)
                                .map(|hunk| hunk.lines.len())
                                .sum()
                        })
                        .unwrap_or(0);
                self.set_current_review_location(repo_id, file, visible);
            }
            ReviewRow::Fold {
                repo_id,
                file,
                hunk,
                line,
                ..
            } => {
                let visible = self
                    .work_item
                    .repos
                    .iter()
                    .find(|repo| repo.record.id == *repo_id)
                    .and_then(|repo| {
                        repo.diff.files.iter().find(|candidate| {
                            candidate.path().to_string_lossy() == file.as_str()
                        })
                    })
                    .map(|file| {
                        file.hunks
                            .iter()
                            .take(*hunk)
                            .map(|hunk| hunk.lines.len())
                            .sum::<usize>()
                            .saturating_add(*line)
                    })
                    .unwrap_or(0);
                self.set_current_review_location(repo_id, file, visible);
            }
            ReviewRow::Source { .. } | ReviewRow::Annotation { .. } => {}
        }
    }

    fn set_current_review_location(&mut self, repo_id: &str, file: &str, visible_line: usize) {
        let Some((repo_index, repo)) = self
            .work_item
            .repos
            .iter()
            .enumerate()
            .find(|(_, repo)| repo.record.id == repo_id)
        else {
            return;
        };
        let Some(file_index) = repo
            .diff
            .files
            .iter()
            .position(|candidate| candidate.path().to_string_lossy() == file)
        else {
            return;
        };
        self.repo_index = repo_index;
        self.file_index = file_index;
        self.cursor = visible_line.min(
            repo.diff.files[file_index]
                .visible_lines()
                .count()
                .saturating_sub(1),
        );
    }

    pub(crate) fn sync_review_cursor_to_current_file(&mut self) {
        let repo_id = self
            .work_item
            .repos
            .get(self.repo_index)
            .map(|repo| repo.record.id.clone());
        let file = self
            .current_file()
            .map(|file| file.path().to_string_lossy().into_owned());
        let (Some(repo_id), Some(file)) = (repo_id, file) else {
            self.review_cursor = 0;
            self.review_scroll = 0;
            return;
        };
        let stream = self.review_stream();
        self.review_cursor = stream
            .rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ReviewRow::Source {
                        repo_id: row_repo,
                        file: path,
                        ..
                    } if row_repo == &repo_id && path == &file
                )
            })
            .or_else(|| {
                stream.rows().iter().position(|row| {
                    matches!(
                        row,
                        ReviewRow::FileHeader {
                            repo_id: row_repo,
                            path,
                            ..
                        } if row_repo == &repo_id && path == &file
                    )
                })
            })
            .unwrap_or(0);
    }

    fn sync_review_cursor_to_current_source(&mut self) {
        let repo_id = self
            .work_item
            .repos
            .get(self.repo_index)
            .map(|repo| repo.record.id.clone());
        let file = self
            .current_file()
            .map(|file| file.path().to_string_lossy().into_owned());
        let (Some(repo_id), Some(file)) = (repo_id, file) else {
            return;
        };
        if let Some(index) = self.review_stream().rows().iter().position(|row| {
            matches!(
                row,
                ReviewRow::Source {
                    repo_id: row_repo,
                    file: path,
                    old_anchor,
                    new_anchor,
                    ..
                } if row_repo == &repo_id
                    && path == &file
                    && new_anchor
                        .as_ref()
                        .or(old_anchor.as_ref())
                        .is_some_and(|anchor| anchor.visible_line == self.cursor)
            )
        }) {
            self.review_cursor = index;
        }
    }

    pub(crate) fn selection(&self) -> (usize, usize) {
        let anchor = self.visual_anchor.unwrap_or(self.cursor);
        (cmp::min(anchor, self.cursor), cmp::max(anchor, self.cursor))
    }

    pub(crate) fn diff_selection(&self) -> DiffSelection {
        let (start_row, end_row) = self.selection();
        DiffSelection { start_row, end_row }
    }

    pub(crate) fn review_selection_mode(&self) -> Option<ReviewSelectionMode> {
        if self.input_mode == InputMode::Visual && self.focus != Focus::Chat {
            self.review_visual_mode
        } else {
            None
        }
    }

    pub(crate) fn review_selection_side(&self) -> Option<AnchorSide> {
        let selection = self.diff_selection();
        self.current_file()
            .and_then(|file| {
                anchor_from_diff(file, selection.start_row, selection.end_row)
                    .ok()
                    .map(|anchor| anchor.side)
            })
            .or_else(|| {
                self.current_file()
                    .and_then(|file| file.visible_lines().nth(self.cursor))
                    .map(|line| {
                        if line.kind == LineKind::Deletion {
                            AnchorSide::Old
                        } else {
                            AnchorSide::New
                        }
                    })
            })
    }

    pub(crate) fn review_row_selection(&self, row: usize) -> Option<ReviewRowSelection> {
        let mode = self.review_selection_mode()?;
        let (start_row, end_row) = self.selection();
        if !(start_row..=end_row).contains(&row) {
            return None;
        }
        match mode {
            ReviewSelectionMode::Line => Some(ReviewRowSelection::Whole),
            ReviewSelectionMode::Block => Some(ReviewRowSelection::Columns {
                start: self
                    .review_visual_anchor_column
                    .min(self.review_visual_column),
                end: self
                    .review_visual_anchor_column
                    .max(self.review_visual_column),
            }),
            ReviewSelectionMode::Character => {
                let anchor = (
                    self.visual_anchor.unwrap_or(self.cursor),
                    self.review_visual_anchor_column,
                );
                let active = (self.cursor, self.review_visual_column);
                let (start, end) = if anchor <= active {
                    (anchor, active)
                } else {
                    (active, anchor)
                };
                if start.0 == end.0 {
                    Some(ReviewRowSelection::Columns {
                        start: start.1,
                        end: end.1,
                    })
                } else if row == start.0 {
                    Some(ReviewRowSelection::Columns {
                        start: start.1,
                        end: usize::MAX,
                    })
                } else if row == end.0 {
                    Some(ReviewRowSelection::Columns {
                        start: 0,
                        end: end.1,
                    })
                } else {
                    Some(ReviewRowSelection::Whole)
                }
            }
        }
    }

    pub(crate) fn has_unsubmitted_work(&self) -> bool {
        self.annotations
            .iter()
            .any(|(annotation, _)| match annotation.kind {
                AnnotationKind::Comment => !annotation.submitted,
                AnnotationKind::Ask => {
                    annotation.delivery_state == crate::domain::DeliveryState::Pending
                }
            })
            || self.pending_context
            || !self.pending_chats.is_empty()
            || !self.pending_comment_ids.is_empty()
            || !self.pending_outbound_ids.is_empty()
            || self.chat.iter().any(|message| message.streaming)
    }

    /// Install a fresh renderer layout while retaining semantic endpoints.
    /// Streaming only changes a message's text and resizing only changes rows,
    /// so both cases are normalized against the rebuilt layout here.
    pub(crate) fn set_chat_layout(&mut self, layout: ChatLayout, display_rows: Vec<usize>) {
        let cursor = if self.chat_autofollow && self.input_mode != InputMode::Visual {
            layout.last_point()
        } else {
            self.chat_navigation
                .as_ref()
                .and_then(|cursor| layout.normalize_point(&cursor.point))
                .or_else(|| {
                    self.chat.get(self.chat_cursor).and_then(|entry| {
                        layout.rows.iter().enumerate().find_map(|(index, row)| {
                            (row.message_id.as_str() == entry.id)
                                .then(|| layout.row_bounds(index))
                                .flatten()
                                .map(|bounds| bounds.0)
                        })
                    })
                })
                .or_else(|| layout.first_point())
        };
        self.chat_navigation = cursor.map(ChatCursor::new);
        self.chat_selection = self.chat_selection.take().and_then(|mut selection| {
            let anchor = layout.normalize_point(&selection.anchor)?;
            let active = layout.normalize_point(&selection.active)?;
            selection.anchor = anchor;
            selection.active = active;
            Some(selection)
        });
        self.chat_layout = Some(layout);
        self.chat_display_rows = display_rows;
        self.sync_chat_cursor();
    }

    pub(crate) fn reset_chat_semantics(&mut self) {
        let restore_main = !self.side_active
            && !self.side_starting
            && self.main_chat.is_none()
            && self.main_chat_viewport.is_some();
        self.chat_layout = None;
        self.chat_navigation = None;
        self.chat_selection = None;
        self.chat_display_rows.clear();
        if self.input_mode == InputMode::Visual && self.focus == Focus::Chat {
            self.input_mode = InputMode::Normal;
        }
        if restore_main {
            self.restore_main_chat_viewport();
        }
        // SIDE teardown used to zero this value even when MAIN still had
        // queued work. Recompute from the visible queue model after every
        // lane swap so progress and :queue agree immediately.
        self.agent_progress.queue_depth = self.queue_entry_ids().len();
    }

    fn enter_chat_surface(&mut self) {
        self.screen = Screen::Chat;
        self.focus = Focus::Chat;
        self.input_mode = InputMode::Normal;
        self.input_return_mode = InputMode::Normal;
        self.compose_target = Some(ComposeTarget::Chat);
    }

    fn capture_main_chat_viewport(&mut self) {
        if self.side_active || self.side_starting {
            return;
        }
        self.main_chat_viewport = Some(ChatViewportState {
            focus: self.focus,
            input_mode: self.input_mode,
            input_return_mode: self.input_return_mode,
            compose: self.compose.clone(),
            compose_cursor: self.compose_cursor,
            compose_scroll: self.compose_scroll,
            compose_target: self.compose_target.clone(),
            chat_cursor: self.chat_cursor,
            chat_scroll: self.chat_scroll,
            chat_total_rows: self.chat_total_rows,
            chat_viewport_rows: self.chat_viewport_rows,
            chat_autofollow: self.chat_autofollow,
            chat_navigation: self.chat_navigation.clone(),
            chat_selection: self.chat_selection.clone(),
            chat_display_rows: self.chat_display_rows.clone(),
        });
    }

    fn restore_main_chat_viewport(&mut self) {
        let Some(snapshot) = self.main_chat_viewport.take() else {
            return;
        };
        self.focus = snapshot.focus;
        self.input_mode = snapshot.input_mode;
        self.input_return_mode = snapshot.input_return_mode;
        self.compose = snapshot.compose;
        self.compose_cursor = snapshot.compose_cursor.min(self.compose.len());
        self.compose_scroll = snapshot.compose_scroll;
        self.compose_target = snapshot.compose_target;
        self.chat_cursor = snapshot.chat_cursor.min(self.chat.len().saturating_sub(1));
        self.chat_scroll = snapshot.chat_scroll;
        self.chat_total_rows = snapshot.chat_total_rows;
        self.chat_viewport_rows = snapshot.chat_viewport_rows;
        self.chat_autofollow = snapshot.chat_autofollow;
        self.chat_navigation = snapshot.chat_navigation;
        self.chat_selection = snapshot.chat_selection;
        self.chat_display_rows = snapshot.chat_display_rows;
    }

    pub(crate) fn chat_selection_mode(&self) -> Option<ChatSelectionMode> {
        self.chat_selection.as_ref().map(|selection| selection.mode)
    }

    fn sync_chat_cursor(&mut self) {
        let Some(point) = self.chat_navigation.as_ref().map(|cursor| &cursor.point) else {
            return;
        };
        if let Some(index) = self
            .chat
            .iter()
            .position(|entry| entry.id == point.message_id.as_str())
        {
            self.chat_cursor = index;
        }
    }

    fn chat_point_for_current_message(&self) -> Option<ChatPoint> {
        let layout = self.chat_layout.as_ref()?;
        let entry = self.chat.get(self.chat_cursor)?;
        layout.rows.iter().enumerate().find_map(|(index, row)| {
            (row.message_id.as_str() == entry.id)
                .then(|| layout.row_bounds(index))
                .flatten()
                .map(|bounds| bounds.0)
        })
    }

    fn enter_chat_visual(&mut self, mode: ChatSelectionMode) -> bool {
        let Some(layout) = self.chat_layout.as_ref() else {
            self.status = "Chat layout is not ready yet; render once and try again".into();
            return false;
        };
        let point = self
            .chat_navigation
            .as_ref()
            .map(|cursor| cursor.point.clone())
            .or_else(|| self.chat_point_for_current_message())
            .or_else(|| layout.first_point());
        let Some(mut point) = point else {
            self.status = "There is no selectable chat text yet".into();
            return false;
        };
        if mode == ChatSelectionMode::Character {
            point = layout
                .locate(&point)
                .and_then(|location| layout.point_for_column(location.row, location.column))
                .unwrap_or(point);
        }
        self.chat_navigation = Some(ChatCursor::new(point.clone()));
        self.chat_selection = match mode {
            ChatSelectionMode::Character => Some(ChatSelection::character(point)),
            ChatSelectionMode::Line => ChatSelection::line(layout, &point),
            ChatSelectionMode::Block => ChatSelection::block(layout, &point),
        };
        if self.chat_selection.is_none() {
            self.status = "There is no selectable chat text at the cursor".into();
            return false;
        }
        self.chat_autofollow = false;
        self.input_mode = InputMode::Visual;
        true
    }

    fn enter_review_visual(&mut self, mode: ReviewSelectionMode) -> bool {
        let mut stream = self.review_stream();
        stream.set_cursor(self.review_cursor);
        if !matches!(stream.current(), Some(ReviewRow::Source { .. })) {
            self.status =
                "Visual selection starts on source rows, not headers or annotation blocks".into();
            return false;
        }
        self.input_mode = InputMode::Visual;
        self.visual_anchor = Some(self.cursor);
        self.review_visual_mode = Some(mode);
        self.review_visual_anchor_column = 0;
        self.review_visual_column = 0;
        self.review_visual_preferred_column = 0;
        self.review_horizontal_scroll = 0;
        self.status = format!(
            "VISUAL {} · h/l columns · j/k rows · y copy · a ask · c comment",
            mode.label()
        );
        true
    }

    fn move_review_visual_horizontal(&mut self, forward: bool) {
        let Some(mode) = self.review_selection_mode() else {
            return;
        };
        if mode == ReviewSelectionMode::Line {
            self.status = "VISUAL LINE spans complete rows · use j/k to extend".into();
            return;
        }
        let Some(text) = self
            .current_file()
            .and_then(|file| file.visible_lines().nth(self.cursor))
            .map(|line| line.content.as_str())
        else {
            return;
        };
        self.review_visual_column = match mode {
            ReviewSelectionMode::Character if forward => {
                next_source_column(text, self.review_visual_column)
            }
            ReviewSelectionMode::Character => {
                previous_source_column(text, self.review_visual_column)
            }
            ReviewSelectionMode::Block if forward => self
                .review_visual_column
                .saturating_add(1)
                .min(cell_width(text).saturating_sub(1)),
            ReviewSelectionMode::Block => self.review_visual_column.saturating_sub(1),
            ReviewSelectionMode::Line => unreachable!(),
        };
        self.review_visual_preferred_column = self.review_visual_column;
        self.ensure_review_column_visible();
    }

    fn move_review_visual_to_edge(&mut self, end: bool) {
        if self.review_selection_mode() == Some(ReviewSelectionMode::Line) {
            self.status = "VISUAL LINE spans complete rows · use j/k to extend".into();
            return;
        }
        let Some(text) = self.current_review_source_text() else {
            return;
        };
        self.review_visual_column = if end {
            source_grapheme_columns(text).last().copied().unwrap_or(0)
        } else {
            0
        };
        self.review_visual_preferred_column = self.review_visual_column;
        self.ensure_review_column_visible();
    }

    fn move_review_visual_word(&mut self, forward: bool) {
        if self.review_selection_mode() == Some(ReviewSelectionMode::Line) {
            self.status = "VISUAL LINE spans complete rows · use j/k to extend".into();
            return;
        }
        let Some(text) = self.current_review_source_text() else {
            return;
        };
        self.review_visual_column = if forward {
            next_source_word_column(text, self.review_visual_column)
        } else {
            previous_source_word_column(text, self.review_visual_column)
        };
        self.review_visual_preferred_column = self.review_visual_column;
        self.ensure_review_column_visible();
    }

    fn current_review_source_text(&self) -> Option<&str> {
        self.current_file()
            .and_then(|file| file.visible_lines().nth(self.cursor))
            .map(|line| line.content.as_str())
    }

    pub(crate) fn set_review_content_width(&mut self, width: usize) {
        self.review_content_width = width.max(1);
        self.ensure_review_column_visible();
    }

    fn ensure_review_column_visible(&mut self) {
        let Some(text) = self.current_review_source_text() else {
            return;
        };
        let total_width = cell_width(text);
        let active_end = source_column_end(text, self.review_visual_column);
        for _ in 0..2 {
            let marker_width = usize::from(self.review_horizontal_scroll > 0)
                + usize::from(active_end < total_width);
            let usable = self
                .review_content_width
                .saturating_sub(marker_width)
                .max(1);
            if self.review_visual_column < self.review_horizontal_scroll {
                self.review_horizontal_scroll = self.review_visual_column;
            } else if active_end > self.review_horizontal_scroll.saturating_add(usable) {
                self.review_horizontal_scroll = active_end.saturating_sub(usable);
            }
        }
    }

    fn sync_review_visual_column(&mut self) {
        if self.review_selection_mode() != Some(ReviewSelectionMode::Character) {
            return;
        }
        let Some(text) = self
            .current_file()
            .and_then(|file| file.visible_lines().nth(self.cursor))
            .map(|line| line.content.as_str())
        else {
            self.review_visual_column = 0;
            return;
        };
        self.review_visual_column = source_grapheme_columns(text)
            .into_iter()
            .take_while(|column| *column <= self.review_visual_preferred_column)
            .last()
            .unwrap_or(0);
        self.ensure_review_column_visible();
    }

    fn move_chat_semantic(&mut self, movement: Movement) -> bool {
        self.move_chat_semantic_inner(movement, true)
    }

    fn move_chat_semantic_without_scrolling(&mut self, movement: Movement) -> bool {
        self.move_chat_semantic_inner(movement, false)
    }

    fn move_chat_semantic_inner(&mut self, movement: Movement, ensure_visible: bool) -> bool {
        let Some(layout) = self.chat_layout.as_ref() else {
            return false;
        };
        let cursor = self
            .chat_navigation
            .clone()
            .or_else(|| self.chat_point_for_current_message().map(ChatCursor::new))
            .or_else(|| layout.first_point().map(ChatCursor::new));
        let Some(cursor) = cursor else {
            return false;
        };
        let next = layout.navigate(&cursor, movement);
        if self.input_mode == InputMode::Visual {
            if let Some(selection) = self.chat_selection.as_mut() {
                selection.extend_to(layout, &next.point);
            }
        }
        self.chat_navigation = Some(next);
        self.chat_autofollow = false;
        self.sync_chat_cursor();
        if ensure_visible {
            self.ensure_chat_navigation_visible();
        }
        true
    }

    fn ensure_chat_navigation_visible(&mut self) {
        let Some(location) = self
            .chat_layout
            .as_ref()
            .zip(self.chat_navigation.as_ref())
            .and_then(|(layout, cursor)| layout.locate(&cursor.point))
        else {
            return;
        };
        let row = self
            .chat_display_rows
            .get(location.row)
            .copied()
            .unwrap_or(location.row);
        if row < self.chat_scroll {
            self.chat_scroll = row;
        } else if row >= self.chat_scroll.saturating_add(self.chat_viewport_rows) {
            self.chat_scroll = row
                .saturating_add(1)
                .saturating_sub(self.chat_viewport_rows.max(1));
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        self.tick(Instant::now());
        if key.code == KeyCode::Esc && self.quit_guard.take().is_some() {
            self.status = "Quit warning dismissed · work remains open".into();
            return Vec::new();
        }
        match self.input_mode {
            InputMode::Command => return self.handle_command_key(key),
            InputMode::Search => return self.handle_search_key(key),
            InputMode::Compose => return self.handle_compose_key(key),
            InputMode::Normal | InputMode::Visual => {}
        }
        self.handle_normal_key(key)
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if self.screen == Screen::Preview {
            return self.handle_preview_key(key);
        }
        if self.screen == Screen::Recovery {
            return self.handle_recovery_key(key);
        }
        if self.screen == Screen::ContextEditor {
            return self.handle_context_key(key);
        }
        if self.screen == Screen::Versions {
            return self.handle_versions_key(key);
        }
        if self.screen == Screen::Prune {
            return self.handle_prune_key(key);
        }
        if self.screen == Screen::Settings {
            return self.handle_settings_key(key);
        }
        if self.screen == Screen::AgentStatus {
            let total = self.agent_progress.timeline.len();
            let page = self.viewport_height.saturating_sub(6).max(1);
            return match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.screen = self.previous_screen;
                    Vec::new()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.scroll = self
                        .scroll
                        .saturating_add(1)
                        .min(self.agent_progress.timeline.len().saturating_sub(1));
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.scroll = self.scroll.saturating_sub(1);
                    Vec::new()
                }
                KeyCode::PageDown | KeyCode::Char('f') if key.modifiers.is_empty() => {
                    self.scroll = self
                        .scroll
                        .saturating_add(page)
                        .min(total.saturating_sub(1));
                    Vec::new()
                }
                KeyCode::PageUp | KeyCode::Char('b') if key.modifiers.is_empty() => {
                    self.scroll = self.scroll.saturating_sub(page);
                    Vec::new()
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.scroll = 0;
                    Vec::new()
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.scroll = total.saturating_sub(1);
                    Vec::new()
                }
                KeyCode::Char('s')
                | KeyCode::Char('c') if key.code == KeyCode::Char('s')
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.request_agent_stop(
                    "Cancellation was requested from Agent Status; waiting for the SDK idle event",
                    )
                }
                _ => Vec::new(),
            };
        }
        if self.screen == Screen::Queue {
            let total = self.queue_entry_ids().len();
            let page = self.viewport_height.max(1);
            return match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.screen = self.previous_screen;
                    Vec::new()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.scroll = self
                        .scroll
                        .saturating_add(1)
                        .min(self.queue_entry_ids().len().saturating_sub(1));
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.scroll = self.scroll.saturating_sub(1);
                    Vec::new()
                }
                KeyCode::PageDown | KeyCode::Char('f') if key.modifiers.is_empty() => {
                    self.scroll = self
                        .scroll
                        .saturating_add(page)
                        .min(total.saturating_sub(1));
                    Vec::new()
                }
                KeyCode::PageUp | KeyCode::Char('b') if key.modifiers.is_empty() => {
                    self.scroll = self.scroll.saturating_sub(page);
                    Vec::new()
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.scroll = 0;
                    Vec::new()
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.scroll = total.saturating_sub(1);
                    Vec::new()
                }
                KeyCode::Char('s') | KeyCode::Char('c')
                    if key.code == KeyCode::Char('s')
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.request_agent_stop(
                        "Cancellation was requested from the queue; waiting for the SDK idle event",
                    )
                }
                KeyCode::Char('d') => {
                    let Some((outbound_id, active)) =
                        self.queue_entry_ids().get(self.scroll).cloned()
                    else {
                        self.status = "No queued prompt is selected".into();
                        return Vec::new();
                    };
                    if active {
                        self.status = "That prompt is active · press s or Ctrl-C to stop it".into();
                        Vec::new()
                    } else {
                        vec![Effect::CancelQueued(outbound_id)]
                    }
                }
                KeyCode::Char('e') => {
                    let Some((outbound_id, active)) =
                        self.queue_entry_ids().get(self.scroll).cloned()
                    else {
                        self.status = "No queued prompt is selected".into();
                        return Vec::new();
                    };
                    if active {
                        self.status =
                            "The active prompt cannot be edited · use :steer or stop it first"
                                .into();
                        return Vec::new();
                    }
                    let text = self
                        .chat
                        .iter()
                        .chain(self.pending_side_entries.iter())
                        .find(|entry| entry.outbound_id.as_deref() == Some(outbound_id.as_str()))
                        .map(|entry| entry.text.clone())
                        .unwrap_or_default();
                    self.screen = Screen::Chat;
                    self.focus = Focus::Chat;
                    self.input_return_mode = InputMode::Normal;
                    self.input_mode = InputMode::Compose;
                    self.compose_target = Some(ComposeTarget::EditQueued(outbound_id));
                    self.compose = text;
                    self.compose_cursor = self.compose.len();
                    self.compose_scroll = 0;
                    self.status =
                        "INSERT · editing queued prompt · Enter replaces it atomically".into();
                    Vec::new()
                }
                _ => Vec::new(),
            };
        }
        if self.screen == Screen::ModelPicker {
            return self.handle_model_picker_key(key);
        }
        if self.pending_prefix == "ctrl-w" {
            match key.code {
                KeyCode::Left => return self.handle_prefix_key('h'),
                KeyCode::Right => return self.handle_prefix_key('l'),
                KeyCode::Down => return self.handle_prefix_key('j'),
                KeyCode::Up => return self.handle_prefix_key('k'),
                KeyCode::Esc => {
                    self.pending_prefix.clear();
                    self.pending_prefix_started = None;
                    self.status = "CTRL-W focus navigation cancelled".into();
                    return Vec::new();
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.handle_control_key(key.code);
        }
        if !self.pending_prefix.is_empty() {
            if let KeyCode::Char(character) = key.code {
                return self.handle_prefix_key(character);
            }
            let prefix = std::mem::take(&mut self.pending_prefix);
            self.pending_prefix.clear();
            self.pending_prefix_started = None;
            self.status = if key.code == KeyCode::Esc {
                format!("KEY {prefix} cancelled · no command was run")
            } else {
                format!("KEY {prefix} cancelled by a non-character key")
            };
            return Vec::new();
        }
        match key.code {
            KeyCode::Char(':') => {
                self.input_return_mode = self.input_mode;
                self.input_mode = InputMode::Command;
                self.command.clear();
                self.command_index = 0;
                self.command_scroll = 0;
                self.status = "COMMAND mode · type to filter, ↑/↓ choose, Tab complete".into();
            }
            KeyCode::Char('/') => {
                self.input_return_mode = self.input_mode;
                self.input_mode = InputMode::Search;
                self.search.clear();
            }
            KeyCode::Tab if self.input_mode == InputMode::Normal => self.toggle_review_chat(),
            KeyCode::Char('t') if self.screen == Screen::Review => {
                self.picker_open = !self.picker_open;
                self.focus = if self.picker_open {
                    Focus::FilePicker
                } else {
                    Focus::Diff
                };
                self.clear_visual_selection();
                self.status = if self.picker_open {
                    "File tree opened · Focus: files → diff".into()
                } else {
                    "File tree closed · Focus: diff".into()
                };
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(1),
            KeyCode::Char('h') if self.focus == Focus::Chat => {
                self.move_chat_semantic(Movement::Left);
            }
            KeyCode::Char('l') if self.focus == Focus::Chat => {
                self.move_chat_semantic(Movement::Right);
            }
            KeyCode::Char('0') if self.focus == Focus::Chat => {
                self.move_chat_semantic(Movement::Home);
            }
            KeyCode::Char('$') if self.focus == Focus::Chat => {
                self.move_chat_semantic(Movement::End);
            }
            KeyCode::Char('w') if self.focus == Focus::Chat => {
                self.move_chat_semantic(Movement::WordForward);
            }
            KeyCode::Char('b') if self.focus == Focus::Chat => {
                self.move_chat_semantic(Movement::WordBackward);
            }
            KeyCode::Char('0')
                if self.focus == Focus::Diff && self.input_mode == InputMode::Visual =>
            {
                self.move_review_visual_to_edge(false);
            }
            KeyCode::Char('$')
                if self.focus == Focus::Diff && self.input_mode == InputMode::Visual =>
            {
                self.move_review_visual_to_edge(true);
            }
            KeyCode::Char('w')
                if self.focus == Focus::Diff && self.input_mode == InputMode::Visual =>
            {
                self.move_review_visual_word(true);
            }
            KeyCode::Char('b')
                if self.focus == Focus::Diff && self.input_mode == InputMode::Visual =>
            {
                self.move_review_visual_word(false);
            }
            KeyCode::Char('h')
                if self.focus == Focus::Diff && self.input_mode == InputMode::Visual =>
            {
                self.move_review_visual_horizontal(false);
            }
            KeyCode::Char('l')
                if self.focus == Focus::Diff && self.input_mode == InputMode::Visual =>
            {
                self.move_review_visual_horizontal(true);
            }
            KeyCode::Char('h') if self.focus == Focus::Diff => self.previous_file(),
            KeyCode::Char('l') if self.focus == Focus::Diff => self.next_file(),
            KeyCode::Char('h') if self.focus == Focus::FilePicker => self.collapse_current_repo(),
            KeyCode::Char('l') if self.focus == Focus::FilePicker => self.expand_current_repo(),
            KeyCode::Char('G') => self.jump_bottom(),
            KeyCode::Char('v') if self.focus == Focus::Chat => {
                self.enter_chat_visual(ChatSelectionMode::Character);
            }
            KeyCode::Char('V') if self.focus == Focus::Chat => {
                self.enter_chat_visual(ChatSelectionMode::Line);
            }
            KeyCode::Char('v')
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk) =>
            {
                self.enter_review_visual(ReviewSelectionMode::Character);
            }
            KeyCode::Char('V')
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk) =>
            {
                self.enter_review_visual(ReviewSelectionMode::Line);
            }
            KeyCode::Esc if self.input_mode == InputMode::Visual => {
                self.clear_visual_selection();
            }
            KeyCode::Esc if !self.search.is_empty() => {
                self.search.clear();
                self.status = "Search cleared".into();
            }
            KeyCode::Char('a') if self.screen == Screen::Review => {
                self.start_composing(AnnotationKind::Ask);
            }
            KeyCode::Char('c') if self.screen == Screen::Review => {
                self.start_composing(AnnotationKind::Comment);
            }
            KeyCode::Char('i')
                if self.screen == Screen::Review && self.input_mode == InputMode::Normal =>
            {
                if matches!(
                    self.compose_target,
                    Some(
                        ComposeTarget::Annotation(_)
                            | ComposeTarget::FollowUp(_)
                            | ComposeTarget::EditAnnotation(_)
                            | ComposeTarget::EditAskMessage { .. }
                    )
                ) {
                    self.input_mode = InputMode::Compose;
                    self.compose_cursor = self.compose_cursor.min(self.compose.len());
                    self.status = "INSERT · resumed preserved inline draft".into();
                } else if let Some((kind, annotation_id, text)) =
                    self.annotation_under_cursor().map(|(annotation, _)| {
                        (
                            annotation.kind,
                            annotation.id.clone(),
                            annotation.text.clone().unwrap_or_default(),
                        )
                    })
                {
                    if kind == AnnotationKind::Ask && self.side_active {
                        self.status =
                            "MAIN Ask is unavailable while SIDE is active · use /main first".into();
                        return Vec::new();
                    }
                    self.input_return_mode = InputMode::Normal;
                    self.input_mode = InputMode::Compose;
                    self.compose_cursor = 0;
                    self.compose_scroll = 0;
                    self.compose.clear();
                    match kind {
                        AnnotationKind::Ask => {
                            self.collapsed_annotations.remove(&annotation_id);
                            self.compose_target = Some(ComposeTarget::FollowUp(annotation_id));
                            self.focus = Focus::InlineAsk;
                            self.status = "INSERT · typing a new Ask follow-up".into();
                        }
                        AnnotationKind::Comment => {
                            self.compose = text;
                            self.compose_cursor = self.compose.len();
                            self.compose_target =
                                Some(ComposeTarget::EditAnnotation(annotation_id));
                            self.status = "INSERT · editing the inline comment".into();
                        }
                    }
                    self.focus_inline_composer();
                } else {
                    self.status = "Move onto an inline annotation prompt before pressing i".into();
                }
            }
            KeyCode::Char('i')
                if self.screen == Screen::Chat && self.input_mode == InputMode::Normal =>
            {
                self.input_return_mode = InputMode::Normal;
                self.input_mode = InputMode::Compose;
                if !matches!(self.compose_target, Some(ComposeTarget::EditQueued(_))) {
                    self.compose_target = Some(ComposeTarget::Chat);
                }
                self.compose_cursor = self.compose.len();
            }
            KeyCode::Enter if self.focus == Focus::FilePicker => {
                self.picker_open = false;
                self.focus = Focus::Diff;
                self.clear_visual_selection();
            }
            KeyCode::Enter if self.screen == Screen::Review => {
                if let Some((kind, annotation_id)) = self
                    .annotation_under_cursor()
                    .map(|(annotation, _)| (annotation.kind, annotation.id.clone()))
                {
                    if kind == AnnotationKind::Ask {
                        if self.side_active {
                            self.status =
                                "MAIN Ask is unavailable while SIDE is active · use /main first"
                                    .into();
                            return Vec::new();
                        }
                        self.collapsed_annotations.remove(&annotation_id);
                        self.input_return_mode = InputMode::Normal;
                        self.input_mode = InputMode::Compose;
                        self.compose_target = Some(ComposeTarget::FollowUp(annotation_id));
                        self.compose.clear();
                        self.compose_cursor = 0;
                        self.focus = Focus::InlineAsk;
                        self.focus_inline_composer();
                    }
                }
            }
            KeyCode::Char('e') if self.screen == Screen::Review => {
                if let Some((kind, annotation_id, text, ambiguous, outdated)) = self
                    .annotation_under_cursor()
                    .map(|(annotation, placement)| {
                        (
                            annotation.kind,
                            annotation.id.clone(),
                            annotation.text.clone().unwrap_or_default(),
                            placement.ambiguous,
                            placement.outdated,
                        )
                    })
                {
                    if ambiguous || outdated {
                        let selection = self.diff_selection();
                        self.clear_visual_selection();
                        return vec![Effect::RepinAnnotation {
                            annotation_id,
                            selection,
                        }];
                    } else if kind == AnnotationKind::Comment {
                        self.input_return_mode = InputMode::Normal;
                        self.input_mode = InputMode::Compose;
                        self.compose_target = Some(ComposeTarget::EditAnnotation(annotation_id));
                        self.compose = text;
                        self.compose_cursor = self.compose.len();
                        self.focus_inline_composer();
                    } else {
                        self.input_return_mode = InputMode::Normal;
                        self.input_mode = InputMode::Compose;
                        if let Some(message) =
                            self.ask_threads.get(&annotation_id).and_then(|thread| {
                                thread.iter().rev().find(|message| message.role == "user")
                            })
                        {
                            self.compose_target = Some(ComposeTarget::EditAskMessage {
                                annotation_id,
                                message_id: message.id.clone(),
                            });
                            self.compose = message.text.clone();
                            self.compose_cursor = self.compose.len();
                        } else {
                            self.compose_target = Some(ComposeTarget::FollowUp(annotation_id));
                            self.compose.clear();
                            self.compose_cursor = 0;
                        }
                        self.focus_inline_composer();
                    }
                }
            }
            KeyCode::Char('u') if self.screen == Screen::Review => {
                return vec![Effect::UndoAnnotation];
            }
            KeyCode::Char('n') => self.jump_to_search(),
            KeyCode::Char('N') => self.jump_to_search_reverse(),
            KeyCode::Char('y') if self.input_mode == InputMode::Visual => {
                return self.yank_current();
            }
            KeyCode::Char('*') if self.focus == Focus::Diff => {
                if let Some(line) = self
                    .current_file()
                    .and_then(|file| file.visible_lines().nth(self.cursor))
                {
                    self.search = line
                        .content
                        .split(|character: char| !character.is_alphanumeric() && character != '_')
                        .find(|word| !word.is_empty())
                        .unwrap_or("")
                        .to_owned();
                    self.jump_to_search();
                }
            }
            KeyCode::Char('o')
                if self.screen == Screen::Review
                    && self.focus == Focus::Diff
                    && self.cursor_on_fold() =>
            {
                return vec![Effect::ExpandContext { all: false }];
            }
            KeyCode::Char('O')
                if self.screen == Screen::Review
                    && self.focus == Focus::Diff
                    && self.cursor_on_fold() =>
            {
                return vec![Effect::ExpandContext { all: true }];
            }
            KeyCode::Char(character @ ('g' | ']' | '[' | 'y' | 'd' | 'z')) => {
                return self.handle_prefix_key(character);
            }
            KeyCode::Char('q') => {
                if !matches!(self.screen, Screen::Review | Screen::Chat) {
                    self.screen = self.previous_screen;
                } else {
                    self.status = "Use :q to quit".into();
                }
            }
            KeyCode::PageDown if self.focus == Focus::Chat => {
                let amount = self.chat_viewport_rows.max(1);
                if self.input_mode == InputMode::Visual {
                    self.move_chat_semantic(Movement::PageDown(amount));
                } else {
                    self.move_down(amount);
                }
            }
            KeyCode::PageUp if self.focus == Focus::Chat => {
                let amount = self.chat_viewport_rows.max(1);
                if self.input_mode == InputMode::Visual {
                    self.move_chat_semantic(Movement::PageUp(amount));
                } else {
                    self.move_up(amount);
                }
            }
            KeyCode::PageDown
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk)
                    && self.input_mode == InputMode::Normal =>
            {
                self.move_review_stream(StreamMovement::PageDown);
            }
            KeyCode::PageUp
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk)
                    && self.input_mode == InputMode::Normal =>
            {
                self.move_review_stream(StreamMovement::PageUp);
            }
            KeyCode::PageDown => self.move_down(self.viewport_height),
            KeyCode::PageUp => self.move_up(self.viewport_height),
            _ => {
                self.pending_prefix.clear();
                self.pending_prefix_started = None;
            }
        }
        Vec::new()
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if self.pending_prefix == "preview-g" {
            self.pending_prefix.clear();
            self.pending_prefix_started = None;
            if key.code == KeyCode::Char('g') {
                self.preview_scroll = 0;
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.close_preview();
            }
            KeyCode::Char('o') if key.modifiers.is_empty() => {
                return vec![Effect::PreviewBrowser];
            }
            KeyCode::Char('j') | KeyCode::Down if key.modifiers.is_empty() => {
                self.preview_scroll = self.preview_scroll.saturating_add(1);
                self.clamp_preview_scroll();
            }
            KeyCode::Char('k') | KeyCode::Up if key.modifiers.is_empty() => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1);
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.preview_scroll = self
                    .preview_scroll
                    .saturating_add((self.viewport_height / 2).max(1));
                self.clamp_preview_scroll();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.preview_scroll = self
                    .preview_scroll
                    .saturating_sub((self.viewport_height / 2).max(1));
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.preview_scroll = self
                    .preview_scroll
                    .saturating_add(self.viewport_height.max(1));
                self.clamp_preview_scroll();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.preview_scroll = self
                    .preview_scroll
                    .saturating_sub(self.viewport_height.max(1));
            }
            KeyCode::Char('G') => {
                self.preview_scroll = self
                    .preview_total_rows
                    .saturating_sub(self.viewport_height.max(1));
            }
            KeyCode::Char('g') => {
                self.pending_prefix = "preview-g".into();
                self.pending_prefix_started = Some(Instant::now());
            }
            _ => {
                if self.pending_prefix == "preview-g" {
                    if key.code == KeyCode::Char('g') {
                        self.preview_scroll = 0;
                    } else {
                        self.pending_prefix.clear();
                        self.pending_prefix_started = None;
                    }
                }
            }
        }
        Vec::new()
    }

    fn clamp_preview_scroll(&mut self) {
        self.preview_scroll = self.preview_scroll.min(
            self.preview_total_rows
                .saturating_sub(self.viewport_height.max(1)),
        );
    }

    pub(crate) fn open_preview(&mut self, markdown: String) {
        if self.screen == Screen::Preview {
            return;
        }
        self.preview_return = Some(PreviewReturnState {
            screen: self.screen,
            focus: self.focus,
            input_mode: self.input_mode,
            compose: self.compose.clone(),
            compose_cursor: self.compose_cursor,
            compose_scroll: self.compose_scroll,
            compose_target: self.compose_target.clone(),
            status: self.status.clone(),
        });
        self.preview_markdown = Some(markdown);
        self.preview_scroll = 0;
        self.preview_total_rows = 0;
        self.screen = Screen::Preview;
        self.input_mode = InputMode::Normal;
        self.pending_prefix.clear();
        self.pending_prefix_started = None;
        self.status = "Markdown preview open · j/k scroll · q/Esc close".into();
    }

    fn close_preview(&mut self) {
        let Some(previous) = self.preview_return.take() else {
            self.screen = Screen::Review;
            self.focus = Focus::Diff;
            self.input_mode = InputMode::Normal;
            return;
        };
        self.screen = previous.screen;
        self.focus = previous.focus;
        self.input_mode = previous.input_mode;
        self.compose = previous.compose;
        self.compose_cursor = previous.compose_cursor;
        self.compose_scroll = previous.compose_scroll;
        self.compose_target = previous.compose_target;
        self.status = previous.status;
        self.preview_markdown = None;
        self.preview_scroll = 0;
        self.preview_total_rows = 0;
        self.pending_prefix.clear();
        self.pending_prefix_started = None;
    }

    fn handle_prune_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.prune_index = cmp::min(
                    self.prune_items.len().saturating_sub(1),
                    self.prune_index.saturating_add(1),
                );
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.prune_index = self.prune_index.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Char(' ') => {
                if let Some(item) = self.prune_items.get_mut(self.prune_index) {
                    if item.id == self.work_item.item.id {
                        self.status =
                            "The open Work Item cannot be selected for pruning; switch first"
                                .into();
                    } else {
                        item.selected = !item.selected;
                    }
                }
                Vec::new()
            }
            KeyCode::Char('d' | 'x') => {
                let ids = self
                    .prune_items
                    .iter()
                    .filter(|item| item.selected)
                    .map(|item| item.id.clone())
                    .collect::<Vec<_>>();
                if ids.is_empty() {
                    self.status = "Select at least one reviewed Work Item".into();
                    Vec::new()
                } else {
                    vec![Effect::PruneWorkItems {
                        ids,
                        export_first: key.code == KeyCode::Char('x'),
                    }]
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = self.previous_screen;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        const SETTING_COUNT: usize = 12;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.settings_index = cmp::min(
                    SETTING_COUNT.saturating_sub(1),
                    self.settings_index.saturating_add(1),
                );
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.settings_index = self.settings_index.saturating_sub(1);
            }
            KeyCode::Enter => {
                if self.settings_index == 0 {
                    self.open_overlay(Screen::ModelPicker);
                    self.model_picker_stage = ModelPickerStage::Model;
                    self.model_picker_index = 0;
                    self.pending_model_selection = None;
                    self.status = "Loading runtime model capabilities…".into();
                    return vec![Effect::LoadModels];
                }
                let target = match self.settings_index {
                    3 => Some((
                        ComposeTarget::SettingBase,
                        self.work_item
                            .repos
                            .get(self.repo_index)
                            .and_then(|repo| repo.record.base_branch.clone())
                            .unwrap_or_default(),
                    )),
                    7 => Some((
                        ComposeTarget::SettingExpandStep,
                        self.expand_step.to_string(),
                    )),
                    _ => None,
                };
                if let Some((target, value)) = target {
                    self.input_return_mode = InputMode::Normal;
                    self.input_mode = InputMode::Compose;
                    self.compose_target = Some(target);
                    self.compose = value;
                    self.compose_cursor = self.compose.len();
                    self.status = "Edit value and press Ctrl-S or Ctrl-Enter to save".into();
                } else {
                    return match self.settings_index {
                        2 => vec![Effect::SetDefaultDiffLayout(match self.default_layout {
                            DiffLayout::Unified => DiffLayout::Split,
                            DiffLayout::Split => DiffLayout::Unified,
                        })],
                        8 => vec![Effect::SetFileTreeDefault(!self.file_tree_default_open)],
                        9 => vec![Effect::SetMarkdownPreview(match self.markdown_preview {
                            MarkdownPreview::Inline => MarkdownPreview::Browser,
                            MarkdownPreview::Browser => MarkdownPreview::Inline,
                        })],
                        _ => {
                            self.status = "This setting is informational".into();
                            Vec::new()
                        }
                    };
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = self.previous_screen;
            }
            _ => {}
        }
        Vec::new()
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let count = match self.model_picker_stage {
            ModelPickerStage::Model => self.model_options.len(),
            ModelPickerStage::Reasoning => self
                .pending_model_selection
                .as_ref()
                .and_then(|selection| {
                    self.model_options
                        .iter()
                        .find(|model| model.id == selection.model_id)
                })
                .map_or(0, |model| model.supported_reasoning_efforts.len()),
            ModelPickerStage::Context => self
                .pending_model_selection
                .as_ref()
                .and_then(|selection| {
                    self.model_options
                        .iter()
                        .find(|model| model.id == selection.model_id)
                })
                .map_or(0, |model| model.context_tiers.len()),
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if count > 0 => {
                self.model_picker_index =
                    (self.model_picker_index + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up if count > 0 => {
                self.model_picker_index = self.model_picker_index.saturating_sub(1);
            }
            KeyCode::Enter => match self.model_picker_stage {
                ModelPickerStage::Model => {
                    let Some(model) = self.model_options.get(self.model_picker_index).cloned()
                    else {
                        self.status = "No runtime models are available".into();
                        return Vec::new();
                    };
                    let has_reasoning = !model.supported_reasoning_efforts.is_empty();
                    let has_context = !model.context_tiers.is_empty();
                    self.pending_model_selection = Some(ModelSelection {
                        model_id: model.id,
                        reasoning_effort: model.default_reasoning_effort.clone(),
                        context_tier: None,
                    });
                    if !has_reasoning && !has_context {
                        let selection = self
                            .pending_model_selection
                            .take()
                            .expect("model selection was just created");
                        self.screen = self.previous_screen;
                        self.status =
                            "Model uses runtime-default reasoning and context settings".into();
                        return vec![Effect::SelectModel(selection)];
                    }
                    if !has_reasoning {
                        self.model_picker_stage = ModelPickerStage::Context;
                        self.model_picker_index = 0;
                    } else {
                        self.model_picker_stage = ModelPickerStage::Reasoning;
                        self.model_picker_index = model
                            .default_reasoning_effort
                            .as_ref()
                            .and_then(|default| {
                                model
                                    .supported_reasoning_efforts
                                    .iter()
                                    .position(|effort| effort == default)
                            })
                            .unwrap_or(0);
                    }
                }
                ModelPickerStage::Reasoning => {
                    let Some(selection) = self.pending_model_selection.as_mut() else {
                        return Vec::new();
                    };
                    let Some(model) = self
                        .model_options
                        .iter()
                        .find(|model| model.id == selection.model_id)
                    else {
                        return Vec::new();
                    };
                    selection.reasoning_effort = model
                        .supported_reasoning_efforts
                        .get(self.model_picker_index)
                        .cloned();
                    if model.context_tiers.is_empty() {
                        let selection = self
                            .pending_model_selection
                            .take()
                            .expect("reasoning stage requires a pending selection");
                        self.screen = self.previous_screen;
                        self.status = "Model has no additional context-tier choice".into();
                        return vec![Effect::SelectModel(selection)];
                    }
                    self.model_picker_stage = ModelPickerStage::Context;
                    self.model_picker_index = 0;
                }
                ModelPickerStage::Context => {
                    let Some(mut selection) = self.pending_model_selection.clone() else {
                        return Vec::new();
                    };
                    let Some(model) = self
                        .model_options
                        .iter()
                        .find(|model| model.id == selection.model_id)
                    else {
                        return Vec::new();
                    };
                    selection.context_tier = model
                        .context_tiers
                        .get(self.model_picker_index)
                        .map(|tier| tier.id.clone());
                    self.screen = self.previous_screen;
                    self.pending_model_selection = None;
                    return vec![Effect::SelectModel(selection)];
                }
            },
            KeyCode::Esc => match self.model_picker_stage {
                ModelPickerStage::Context => {
                    let has_reasoning = self
                        .pending_model_selection
                        .as_ref()
                        .and_then(|selection| {
                            self.model_options
                                .iter()
                                .find(|model| model.id == selection.model_id)
                        })
                        .is_some_and(|model| !model.supported_reasoning_efforts.is_empty());
                    self.model_picker_stage = if has_reasoning {
                        ModelPickerStage::Reasoning
                    } else {
                        ModelPickerStage::Model
                    };
                    self.model_picker_index = 0;
                    self.status = "Back to the previous model choice".into();
                }
                ModelPickerStage::Reasoning => {
                    self.model_picker_stage = ModelPickerStage::Model;
                    self.model_picker_index = self
                        .pending_model_selection
                        .as_ref()
                        .and_then(|selection| {
                            self.model_options
                                .iter()
                                .position(|model| model.id == selection.model_id)
                        })
                        .unwrap_or(0);
                    self.status = "Back to model selection".into();
                }
                ModelPickerStage::Model => {
                    self.screen = self.previous_screen;
                    self.pending_model_selection = None;
                }
            },
            KeyCode::Char('q') => {
                self.screen = self.previous_screen;
                self.pending_model_selection = None;
            }
            _ => {}
        }
        Vec::new()
    }

    fn handle_versions_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.version_index = cmp::min(
                    self.versions.len().saturating_sub(1),
                    self.version_index.saturating_add(1),
                );
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.version_index = self.version_index.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Enter => self
                .versions
                .get(self.version_index)
                .map(|choice| Effect::OpenVersion {
                    repo_id: choice.version.repo_id.clone(),
                    version_id: choice.version.id.clone(),
                })
                .into_iter()
                .collect(),
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = self.previous_screen;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_prefix_key(&mut self, character: char) -> Vec<Effect> {
        let prefix = std::mem::take(&mut self.pending_prefix);
        self.pending_prefix_started = None;
        if !prefix.is_empty() {
            self.status = format!("KEY {prefix}{character} · command complete");
        }
        match (prefix.as_str(), character) {
            ("g", 'g') => self.jump_top(),
            ("g", 'c') if self.input_mode == InputMode::Normal => {
                self.screen = Screen::Chat;
                self.focus = Focus::Chat;
                self.clear_visual_selection();
            }
            ("g", 'r') if self.input_mode == InputMode::Normal => {
                self.screen = Screen::Review;
                self.focus = Focus::Diff;
                self.clear_visual_selection();
            }
            ("g", 'm') => {
                return vec![match self.markdown_preview {
                    MarkdownPreview::Inline => Effect::Preview,
                    MarkdownPreview::Browser => Effect::PreviewBrowser,
                }]
            }
            ("ctrl-w", direction @ ('h' | 'j' | 'k' | 'l')) => {
                self.move_review_focus(direction);
            }
            ("]", 'a') => self.jump_annotation(true),
            ("[", 'a') => self.jump_annotation(false),
            ("y", 'y') => return self.yank_current(),
            ("d", 'd') => {
                if let Some((annotation, _)) = self.annotation_under_cursor() {
                    return vec![Effect::DeleteAnnotation(annotation.id.clone())];
                }
            }
            ("z", 'a') => {
                if let Some(annotation_id) = self
                    .annotation_under_cursor()
                    .map(|(annotation, _)| annotation.id.clone())
                {
                    if !self.collapsed_annotations.remove(&annotation_id) {
                        self.collapsed_annotations.insert(annotation_id.clone());
                    }
                    self.review_cursor = self
                        .review_stream()
                        .rows()
                        .iter()
                        .position(|row| {
                            matches!(
                                row,
                                ReviewRow::Annotation { block, .. }
                                    if block.annotation_id == annotation_id
                                        && matches!(
                                            block.part,
                                            crate::review_stream::AnnotationRowPart::Header { .. }
                                        )
                            )
                        })
                        .unwrap_or(self.review_cursor);
                    self.status = "Toggled annotation fold".into();
                }
            }
            ("", 'y') if self.input_mode == InputMode::Visual => return self.yank_current(),
            ("", start @ ('g' | ']' | '[' | 'y' | 'd' | 'z')) => {
                self.pending_prefix = start.to_string();
                self.pending_prefix_started = Some(Instant::now());
                self.status = match start {
                    'g' => "KEY g · waiting for g/c/r/m · Esc cancel".into(),
                    ']' => "KEY ] · waiting for a · Esc cancel".into(),
                    '[' => "KEY [ · waiting for a · Esc cancel".into(),
                    'y' => "KEY y · waiting for y · Esc cancel".into(),
                    'd' => "KEY d · waiting for d · Esc cancel".into(),
                    'z' => "KEY z · waiting for a · Esc cancel".into(),
                    _ => unreachable!(),
                };
            }
            _ if !prefix.is_empty() => {
                self.status = format!("Unknown key chord: {prefix}{character}");
            }
            _ => {}
        }
        Vec::new()
    }

    fn handle_context_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = self.previous_screen;
                Vec::new()
            }
            KeyCode::Char('r') => {
                self.context_draft.clear();
                self.context_streaming = true;
                vec![Effect::GenerateContext]
            }
            KeyCode::Char('a') if !self.context_draft.trim().is_empty() => {
                vec![Effect::AttachContext(self.context_draft.clone())]
            }
            KeyCode::Char('e') => {
                self.input_return_mode = InputMode::Normal;
                self.input_mode = InputMode::Compose;
                self.compose_target = Some(ComposeTarget::Context);
                self.compose = self.context_draft.clone();
                self.compose_cursor = self.compose.len();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_recovery_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.recovery_index = cmp::min(
                    self.recovery_count().saturating_sub(1),
                    self.recovery_index.saturating_add(1),
                );
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.recovery_index = self.recovery_index.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Char('r') => self.recovery_effect(true).into_iter().collect(),
            KeyCode::Char('d') => self.recovery_effect(false).into_iter().collect(),
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = Screen::Review;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn recovery_effect(&self, resend: bool) -> Option<Effect> {
        if let Some(message) = self.pending_asks.get(self.recovery_index) {
            return Some(if resend {
                Effect::ResendPendingAsk(message.clone())
            } else {
                Effect::DiscardPendingAsk(message.id.clone())
            });
        }
        let mut index = self.recovery_index.saturating_sub(self.pending_asks.len());
        if let Some(chat) = self.pending_chats.get(index) {
            return Some(if resend {
                Effect::ResendPendingChat(chat.clone())
            } else {
                Effect::DiscardPendingChat(chat.id.clone())
            });
        }
        index = index.saturating_sub(self.pending_chats.len());
        if !self.pending_comment_ids.is_empty() {
            if index == 0 {
                return Some(if resend {
                    Effect::ResendPendingComments
                } else {
                    Effect::DiscardPendingComments
                });
            }
            index = index.saturating_sub(1);
        }
        (self.pending_context && index == 0).then_some(if resend {
            Effect::ResendPendingContext
        } else {
            Effect::DiscardPendingContext
        })
    }

    fn recovery_count(&self) -> usize {
        self.pending_asks.len()
            + self.pending_chats.len()
            + usize::from(!self.pending_comment_ids.is_empty())
            + usize::from(self.pending_context)
    }

    pub(crate) fn queue_entry_ids(&self) -> Vec<(String, bool)> {
        let main_entries = self.main_chat.as_ref().into_iter().flatten().chain(
            (self.main_chat.is_none())
                .then_some(&self.chat)
                .into_iter()
                .flatten(),
        );
        let side_entries = if self.main_chat.is_some() {
            self.chat.iter().chain(self.pending_side_entries.iter())
        } else {
            [].iter().chain(self.pending_side_entries.iter())
        };
        main_entries
            .chain(side_entries)
            .filter_map(|entry| {
                if entry.role == "copilot" {
                    return None;
                }
                let id = entry.outbound_id.as_ref()?;
                self.pending_outbound_ids.contains(id).then(|| {
                    (
                        id.clone(),
                        self.agent_progress.active_outbound_id.as_deref() == Some(id.as_str())
                            && self.agent_progress.phase.is_active(),
                    )
                })
            })
            .collect()
    }

    fn request_agent_stop(&mut self, detail: &str) -> Vec<Effect> {
        if self.agent_progress.phase == AgentPhase::Stopping {
            self.status =
                "Cancellation is already in progress · open :agent-status to verify activity"
                    .into();
            return Vec::new();
        }
        let Some(outbound_id) = self.agent_progress.active_outbound_id.clone() else {
            self.status =
                "No active Copilot response to stop · queued prompts are unchanged".into();
            return Vec::new();
        };
        if !self.agent_progress.phase.is_active() {
            self.status =
                "No active Copilot response to stop · queued prompts are unchanged".into();
            return Vec::new();
        }
        self.agent_progress.record(
            AgentPhase::Stopping,
            "Stopping the active Copilot response",
            detail,
            Some(outbound_id),
        );
        self.status = "Stopping the current Copilot response…".into();
        vec![Effect::AbortAgent]
    }

    fn handle_control_key(&mut self, code: KeyCode) -> Vec<Effect> {
        let page = if self.focus == Focus::Chat {
            self.chat_viewport_rows.max(1)
        } else {
            self.viewport_height.max(1)
        };
        let half = (page / 2).max(1);
        match code {
            KeyCode::Char('v') if self.focus == Focus::Chat => {
                self.enter_chat_visual(ChatSelectionMode::Block);
                return Vec::new();
            }
            KeyCode::Char('v')
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk) =>
            {
                self.enter_review_visual(ReviewSelectionMode::Block);
                return Vec::new();
            }
            KeyCode::Char('c') => {
                if self.side_starting {
                    return vec![Effect::ExitSide];
                }
                if self.agent_progress.phase.is_active()
                    && self.agent_progress.active_outbound_id.is_some()
                {
                    return self.request_agent_stop(
                        "Cancellation was requested; waiting for the SDK idle event",
                    );
                }
                if self.screen == Screen::Chat && !self.compose.is_empty() {
                    self.compose.clear();
                    self.compose_cursor = 0;
                    self.compose_scroll = 0;
                    self.compose_target = None;
                    self.status = "Draft cancelled".into();
                    return Vec::new();
                }
                if self.side_active {
                    return vec![Effect::ExitSide];
                }
                self.status =
                    "Nothing is running · Esc cancels a draft, :agent-status inspects Copilot"
                        .into();
            }
            KeyCode::Char('d')
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk)
                    && self.input_mode == InputMode::Normal =>
            {
                self.move_review_stream(StreamMovement::HalfPageDown);
            }
            KeyCode::Char('u')
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk)
                    && self.input_mode == InputMode::Normal =>
            {
                self.move_review_stream(StreamMovement::HalfPageUp);
            }
            KeyCode::Char('f')
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk)
                    && self.input_mode == InputMode::Normal =>
            {
                self.move_review_stream(StreamMovement::PageDown);
            }
            KeyCode::Char('b')
                if self.screen == Screen::Review
                    && matches!(self.focus, Focus::Diff | Focus::InlineAsk)
                    && self.input_mode == InputMode::Normal =>
            {
                self.move_review_stream(StreamMovement::PageUp);
            }
            KeyCode::Char('d') => self.move_down(half),
            KeyCode::Char('u') => self.move_up(half),
            KeyCode::Char('f') => self.move_down(page),
            KeyCode::Char('b') => self.move_up(page),
            KeyCode::Char('w') => {
                self.pending_prefix = "ctrl-w".into();
                self.pending_prefix_started = Some(Instant::now());
                self.status = "CTRL-W · h/j/k/l or arrows · Esc cancel".into();
            }
            KeyCode::Char('h') if self.pending_prefix == "ctrl-w" => {
                return self.handle_prefix_key('h');
            }
            KeyCode::Char('l') if self.pending_prefix == "ctrl-w" => {
                return self.handle_prefix_key('l');
            }
            KeyCode::Char('j') if self.pending_prefix == "ctrl-w" => {
                return self.handle_prefix_key('j');
            }
            KeyCode::Char('k') if self.pending_prefix == "ctrl-w" => {
                return self.handle_prefix_key('k');
            }
            KeyCode::Char('j' | 'm') => {
                return self.handle_normal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
            _ => {
                self.pending_prefix.clear();
                self.pending_prefix_started = None;
            }
        }
        Vec::new()
    }

    fn handle_command_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if terminal_enter(&key) {
            let matches = command_matches(&self.command);
            let selected = matches.get(self.command_index).map(|(command, _)| *command);
            let typed = self.command.trim();
            let exact = COMMANDS
                .iter()
                .map(|(command, _)| *command)
                .find(|command| command_seed(command) == typed);
            let command = if typed.is_empty() {
                selected.map(command_seed).unwrap_or_default()
            } else if exact.is_some() {
                typed.to_owned()
            } else if selected
                .is_some_and(|suggestion| literal_command_completion(typed, suggestion))
            {
                selected.map(command_seed).unwrap_or_else(|| typed.to_owned())
            } else if !typed.contains(char::is_whitespace) {
                selected
                    .map(command_seed)
                    .unwrap_or_else(|| typed.to_owned())
            } else {
                typed.to_owned()
            };
            if exact.is_none()
                && selected.is_some_and(|suggestion| suggestion.contains('<'))
                && !typed.contains(char::is_whitespace)
            {
                self.command = format!("{command} ");
                self.command_index = 0;
                self.command_scroll = 0;
                return Vec::new();
            }
            self.command.clear();
            let restore_visual = self.input_return_mode == InputMode::Visual
                && matches!(command.trim(), "diff split" | "diff unified");
            if restore_visual {
                self.input_mode = InputMode::Visual;
            } else {
                if self.input_return_mode == InputMode::Visual {
                    self.clear_visual_selection();
                }
                self.input_mode = InputMode::Normal;
            }
            self.input_return_mode = InputMode::Normal;
            self.status.clear();
            return self.execute_command(command.trim());
        }
        match key.code {
            KeyCode::Esc => {
                self.input_mode = self.input_return_mode;
                self.input_return_mode = InputMode::Normal;
                self.command.clear();
                self.status = if self.input_mode == InputMode::Visual {
                    "VISUAL · command palette closed · selection preserved".into()
                } else {
                    "Command palette closed".into()
                };
                Vec::new()
            }
            KeyCode::Backspace => {
                self.command.pop();
                self.command_index = 0;
                self.command_scroll = 0;
                Vec::new()
            }
            KeyCode::Up => {
                let count = command_matches(&self.command).len();
                if count > 0 {
                    if self.command_index == 0 {
                        self.command_index = count - 1;
                        self.command_scroll = self
                            .command_index
                            .saturating_sub(self.command_viewport_rows.saturating_sub(1));
                    } else {
                        self.command_index -= 1;
                        self.command_scroll = self.command_scroll.min(self.command_index);
                    }
                }
                Vec::new()
            }
            KeyCode::Down => {
                let count = command_matches(&self.command).len();
                let page = self.command_viewport_rows.max(1);
                if count > 0 {
                    self.command_index = (self.command_index + 1) % count;
                    if self.command_index == 0 {
                        self.command_scroll = 0;
                    } else if self.command_index >= self.command_scroll + page {
                        self.command_scroll = self.command_index + 1 - page;
                    }
                }
                Vec::new()
            }
            KeyCode::Home => {
                self.command_index = 0;
                self.command_scroll = 0;
                Vec::new()
            }
            KeyCode::End => {
                let count = command_matches(&self.command).len();
                self.command_index = count.saturating_sub(1);
                self.command_scroll = self
                    .command_index
                    .saturating_sub(self.command_viewport_rows.saturating_sub(1));
                Vec::new()
            }
            KeyCode::PageUp => {
                let page = self.command_viewport_rows.max(1);
                self.command_index = self.command_index.saturating_sub(page);
                self.command_scroll = self.command_scroll.min(self.command_index);
                Vec::new()
            }
            KeyCode::PageDown => {
                let count = command_matches(&self.command).len();
                let page = self.command_viewport_rows.max(1);
                self.command_index = cmp::min(
                    count.saturating_sub(1),
                    self.command_index.saturating_add(page),
                );
                self.command_scroll = self.command_index.saturating_sub(page.saturating_sub(1));
                Vec::new()
            }
            KeyCode::Tab => {
                if let Some((suggestion, _)) =
                    command_matches(&self.command).get(self.command_index)
                {
                    self.command = command_seed(suggestion);
                    if suggestion.contains('<') {
                        self.command.push(' ');
                    }
                    self.command_index = 0;
                    self.command_scroll = 0;
                }
                Vec::new()
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.command.push(character);
                self.command_index = 0;
                self.command_scroll = 0;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if terminal_enter(&key) {
            self.input_mode = self.input_return_mode;
            if self.focus == Focus::FilePicker {
                self.status = if self.search.is_empty() {
                    "File filter cleared".into()
                } else {
                    format!("File filter: {}", self.search)
                };
                return Vec::new();
            }
            if self.focus == Focus::Chat {
                self.jump_to_chat_search(true, true);
                return Vec::new();
            }
            self.jump_to_search();
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc => {
                self.input_mode = self.input_return_mode;
                self.search.clear();
            }
            KeyCode::Backspace => {
                self.search.pop();
                self.select_first_file_search_match();
            }
            KeyCode::Char(character) => {
                self.search.push(character);
                self.select_first_file_search_match();
            }
            _ => {}
        }
        Vec::new()
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if terminal_enter(&key)
            || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return self.submit_compose();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.agent_progress.phase.is_active()
                        && self.agent_progress.active_outbound_id.is_some()
                    {
                        return self.request_agent_stop(
                            "Draft preserved; press Ctrl-C again after Copilot stops to discard it",
                        );
                    }
                    self.compose.clear();
                    self.compose_cursor = 0;
                    self.compose_scroll = 0;
                    self.input_mode = self.input_return_mode;
                    self.compose_target = None;
                    if self.input_mode == InputMode::Visual {
                        self.sync_review_cursor_to_current_source();
                    }
                    self.status = "Draft cancelled".into();
                    return Vec::new();
                }
                KeyCode::Char('a') => {
                    self.compose_cursor = self.compose_line_start();
                    return Vec::new();
                }
                KeyCode::Char('e') => {
                    self.compose_cursor = self.compose_line_end();
                    return Vec::new();
                }
                KeyCode::Char('u') => {
                    let start = self.compose_line_start();
                    self.compose.replace_range(start..self.compose_cursor, "");
                    self.compose_cursor = start;
                    return Vec::new();
                }
                KeyCode::Char('k') => {
                    let end = self.compose_line_end();
                    self.compose.replace_range(self.compose_cursor..end, "");
                    return Vec::new();
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                self.input_mode = self.input_return_mode;
                let preserve = matches!(
                    self.compose_target,
                    Some(ComposeTarget::Chat | ComposeTarget::EditQueued(_))
                ) || (self.input_return_mode == InputMode::Normal
                    && matches!(
                        self.compose_target,
                        Some(
                            ComposeTarget::Annotation(_)
                                | ComposeTarget::FollowUp(_)
                                | ComposeTarget::EditAnnotation(_)
                                | ComposeTarget::EditAskMessage { .. }
                        )
                    ));
                if preserve {
                    self.status = if self.compose.is_empty() {
                        "Composer left in NORMAL mode".into()
                    } else {
                        "Draft kept · i resumes · gm previews · Ctrl-C discards".into()
                    };
                } else {
                    self.compose.clear();
                    self.compose_cursor = 0;
                    self.compose_target = None;
                }
                if self.input_mode == InputMode::Visual {
                    self.sync_review_cursor_to_current_source();
                }
                Vec::new()
            }
            KeyCode::Backspace => {
                if self.compose_cursor > 0 {
                    let previous = self.previous_compose_boundary();
                    self.compose
                        .replace_range(previous..self.compose_cursor, "");
                    self.compose_cursor = previous;
                }
                Vec::new()
            }
            KeyCode::Delete => {
                let next = self.next_compose_boundary();
                if next > self.compose_cursor {
                    self.compose.replace_range(self.compose_cursor..next, "");
                }
                Vec::new()
            }
            KeyCode::Left => {
                self.compose_cursor = self.previous_compose_boundary();
                Vec::new()
            }
            KeyCode::Right => {
                self.compose_cursor = self.next_compose_boundary();
                Vec::new()
            }
            KeyCode::Home => {
                self.compose_cursor = self.compose_line_start();
                Vec::new()
            }
            KeyCode::End => {
                self.compose_cursor = self.compose_line_end();
                Vec::new()
            }
            KeyCode::Up => {
                self.move_compose_vertical(-1);
                Vec::new()
            }
            KeyCode::Down => {
                self.move_compose_vertical(1);
                Vec::new()
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_compose("\n");
                Vec::new()
            }
            KeyCode::Tab => {
                self.insert_compose("    ");
                Vec::new()
            }
            KeyCode::Char(character) => {
                self.insert_compose(&character.to_string());
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> Vec<Effect> {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.input_mode {
            InputMode::Compose => {
                self.insert_compose(&normalized);
                let lines = normalized.lines().count().max(1);
                self.status = format!(
                    "Pasted {} bytes across {lines} line{} · review before submitting",
                    normalized.len(),
                    if lines == 1 { "" } else { "s" }
                );
            }
            InputMode::Command => {
                self.command
                    .push_str(&normalized.replace(['\n', '\t'], " "));
                self.command_index = 0;
                self.command_scroll = 0;
            }
            InputMode::Search => {
                self.search
                    .push_str(normalized.lines().next().unwrap_or_default());
                self.select_first_file_search_match();
            }
            _ => {
                self.status = "Paste ignored in NORMAL mode · press i to edit first".into();
            }
        }
        Vec::new()
    }

    fn submit_compose(&mut self) -> Vec<Effect> {
        if self.compose.trim().is_empty() {
            self.status = "Enter text before submitting".into();
            return Vec::new();
        }
        let text = std::mem::take(&mut self.compose);
        self.compose_cursor = 0;
        self.compose_scroll = 0;
        let target = self
            .compose_target
            .take()
            .expect("compose mode always has a target");
        let selection = self.diff_selection();
        self.clear_review_visual_selection();
        self.input_mode = InputMode::Normal;
        let slash_commands_enabled = matches!(
            &target,
            ComposeTarget::Chat
                | ComposeTarget::Annotation(AnnotationKind::Ask)
                | ComposeTarget::FollowUp(_)
        );
        if slash_commands_enabled {
            let trimmed = text.trim();
            if trimmed == "/side" {
                self.enter_chat_surface();
                self.capture_main_chat_viewport();
                return vec![Effect::StartSide(None)];
            }
            if let Some(question) = trimmed.strip_prefix("/side ") {
                self.enter_chat_surface();
                self.capture_main_chat_viewport();
                return vec![Effect::StartSide(Some(question.trim().to_owned()))];
            }
            if let Some(correction) = trimmed.strip_prefix("/steer ") {
                let correction = correction.trim().to_owned();
                return if self.agent_progress.phase.is_active()
                    && self.agent_progress.active_outbound_id.is_some()
                {
                    vec![Effect::SteerChat(correction)]
                } else {
                    self.status =
                        "No response is active; the correction was queued as a normal prompt"
                            .into();
                    vec![Effect::SendChat(correction)]
                };
            }
            if matches!(trimmed, "/main" | "/side-exit") {
                self.enter_chat_surface();
                return vec![Effect::ExitSide];
            }
        }
        match target {
            ComposeTarget::Annotation(kind) => {
                vec![Effect::CreateAnnotation {
                    kind,
                    text,
                    selection,
                }]
            }
            ComposeTarget::FollowUp(annotation_id) => {
                vec![Effect::FollowUpAsk {
                    annotation_id,
                    text,
                }]
            }
            ComposeTarget::EditAnnotation(annotation_id) => {
                vec![Effect::EditAnnotation {
                    annotation_id,
                    text,
                }]
            }
            ComposeTarget::EditAskMessage {
                annotation_id,
                message_id,
            } => vec![Effect::EditAskMessage {
                annotation_id,
                message_id,
                text,
            }],
            ComposeTarget::EditQueued(outbound_id) => {
                vec![Effect::ReplaceQueued { outbound_id, text }]
            }
            ComposeTarget::Chat => {
                vec![Effect::SendChat(text)]
            }
            ComposeTarget::Context => {
                self.context_draft = text;
                Vec::new()
            }
            ComposeTarget::SettingBase => vec![Effect::SetBase {
                branch: text,
                repo: None,
            }],
            ComposeTarget::SettingExpandStep => match text.parse::<usize>() {
                Ok(step) if step > 0 => vec![Effect::SetExpandStep(step)],
                _ => {
                    self.status = "Expand step must be a positive integer".into();
                    Vec::new()
                }
            },
        }
    }

    fn execute_command(&mut self, command: &str) -> Vec<Effect> {
        let mut parts = command.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("q" | "quit"), _) => vec![Effect::Quit { force: false }],
            (Some("q!" | "quit!"), _) => vec![Effect::Quit { force: true }],
            (Some("diff"), Some("split")) => {
                self.layout = DiffLayout::Split;
                self.status = "Diff layout: split · semantic selection preserved".into();
                Vec::new()
            }
            (Some("diff"), Some("unified")) => {
                self.layout = DiffLayout::Unified;
                self.status = "Diff layout: unified · semantic selection preserved".into();
                Vec::new()
            }
            (Some("diff"), Some("expand")) => {
                vec![Effect::ExpandContext { all: true }]
            }
            (Some("settings"), _) => {
                self.open_overlay(Screen::Settings);
                Vec::new()
            }
            (Some("versions"), _) => {
                self.open_overlay(Screen::Versions);
                Vec::new()
            }
            (Some("prune"), _) => {
                self.open_overlay(Screen::Prune);
                vec![Effect::LoadPrune]
            }
            (Some("generate-context"), _) => {
                self.open_overlay(Screen::ContextEditor);
                self.context_draft.clear();
                self.context_streaming = true;
                vec![Effect::GenerateContext]
            }
            (Some("snapshot"), _) => vec![Effect::Snapshot],
            (Some("preview-browser"), _) => vec![Effect::PreviewBrowser],
            (Some("sync"), _) => vec![Effect::Sync],
            (Some("fork"), _) => vec![Effect::Fork],
            (Some("compact"), _) => {
                let rest = parts.collect::<Vec<_>>().join(" ");
                vec![Effect::Compact((!rest.is_empty()).then_some(rest))]
            }
            (Some("stop" | "abort"), _) => self.request_agent_stop(
                "Cancellation was requested from command mode; waiting for the SDK idle event",
            ),
            (Some("agent-status" | "progress"), _) => {
                self.open_overlay(Screen::AgentStatus);
                Vec::new()
            }
            (Some("queue"), _) => {
                self.open_overlay(Screen::Queue);
                Vec::new()
            }
            (Some("steer"), correction) => {
                let mut words = correction.into_iter().collect::<Vec<_>>();
                words.extend(parts);
                let correction = words.join(" ");
                if correction.is_empty() {
                    self.status = "Usage: :steer <correction>".into();
                    Vec::new()
                } else if self.agent_progress.phase.is_active()
                    && self.agent_progress.active_outbound_id.is_some()
                {
                    vec![Effect::SteerChat(correction)]
                } else {
                    self.status =
                        "No response is active; the correction was queued as a normal prompt"
                            .into();
                    vec![Effect::SendChat(correction)]
                }
            }
            (Some("side"), question) => {
                let mut words = question.into_iter().collect::<Vec<_>>();
                words.extend(parts);
                let question = words.join(" ");
                self.enter_chat_surface();
                self.capture_main_chat_viewport();
                vec![Effect::StartSide(
                    (!question.is_empty()).then_some(question),
                )]
            }
            (Some("main" | "side-exit"), _) => {
                self.enter_chat_surface();
                vec![Effect::ExitSide]
            }
            (Some("model"), _) if self.side_active => {
                self.status =
                    "Model changes are MAIN-scoped · return with /main before using :model".into();
                Vec::new()
            }
            (Some("model"), None) => {
                self.open_overlay(Screen::ModelPicker);
                self.model_picker_stage = ModelPickerStage::Model;
                self.model_picker_index = 0;
                self.pending_model_selection = None;
                self.status = "Loading runtime model capabilities…".into();
                vec![Effect::LoadModels]
            }
            (Some("export"), format) => vec![Effect::Export {
                format: format.map(str::to_owned),
            }],
            (Some("base"), Some(branch)) => {
                let remaining = parts.collect::<Vec<_>>();
                let repo = remaining
                    .windows(2)
                    .find(|window| window[0] == "--repo")
                    .map(|window| window[1].to_owned());
                vec![Effect::SetBase {
                    branch: branch.to_owned(),
                    repo,
                }]
            }
            _ => {
                self.status = format!("Unknown command: :{command}");
                Vec::new()
            }
        }
    }

    fn start_composing(&mut self, kind: AnnotationKind) {
        if kind == AnnotationKind::Ask && self.side_active {
            self.status = "MAIN Ask is unavailable while SIDE is active · use /main first".into();
            return;
        }
        if !matches!(self.focus, Focus::Diff | Focus::InlineAsk) {
            self.status = "Move focus to a code line before creating an annotation".into();
            return;
        }
        if self.current_line_count() == 0 {
            self.status = "Cannot annotate a binary or empty file".into();
            return;
        }
        let mut stream = self.review_stream();
        stream.set_cursor(self.review_cursor);
        if !matches!(stream.current(), Some(ReviewRow::Source { .. })) {
            self.status =
                "Cannot annotate fold and metadata rows, file headers, or hunk headers".into();
            return;
        }
        let Some(file) = self.current_file() else {
            self.status = "Cannot annotate a binary or empty file".into();
            return;
        };
        let selection = self.diff_selection();
        if let Err(error) = anchor_from_diff(file, selection.start_row, selection.end_row) {
            self.status = error.to_string();
            return;
        }
        self.input_return_mode = self.input_mode;
        self.input_mode = InputMode::Compose;
        self.compose_target = Some(ComposeTarget::Annotation(kind));
        self.compose.clear();
        self.compose_cursor = 0;
        self.compose_scroll = 0;
        self.focus_inline_composer();
    }

    fn insert_compose(&mut self, text: &str) {
        self.compose.insert_str(self.compose_cursor, text);
        self.compose_cursor += text.len();
    }

    fn previous_compose_boundary(&self) -> usize {
        previous_grapheme_boundary(&self.compose, self.compose_cursor)
    }

    fn next_compose_boundary(&self) -> usize {
        next_grapheme_boundary(&self.compose, self.compose_cursor)
    }

    fn compose_line_start(&self) -> usize {
        self.compose[..self.compose_cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0)
    }

    fn compose_line_end(&self) -> usize {
        self.compose[self.compose_cursor..]
            .find('\n')
            .map(|index| self.compose_cursor + index)
            .unwrap_or(self.compose.len())
    }

    fn move_compose_vertical(&mut self, direction: i8) {
        let initial_column = matches!(
            self.compose_target,
            Some(
                ComposeTarget::Annotation(_)
                    | ComposeTarget::FollowUp(_)
                    | ComposeTarget::EditAnnotation(_)
                    | ComposeTarget::EditAskMessage { .. }
            )
        )
        .then_some(2)
        .unwrap_or(0);
        let positions =
            editor_cursor_positions(&self.compose, self.compose_wrap_width, initial_column);
        let Some((_, row, column)) = positions
            .iter()
            .find(|(byte, _, _)| *byte == self.compose_cursor)
            .copied()
        else {
            return;
        };
        let target_row = if direction < 0 {
            row.checked_sub(1)
        } else {
            Some(row.saturating_add(1))
        };
        let Some(target_row) = target_row else {
            return;
        };
        if let Some((byte, _, _)) = positions
            .iter()
            .filter(|(_, candidate_row, _)| *candidate_row == target_row)
            .min_by_key(|(_, _, candidate_column)| candidate_column.abs_diff(column))
        {
            self.compose_cursor = *byte;
        }
    }

    fn open_overlay(&mut self, screen: Screen) {
        self.previous_screen = self.screen;
        self.screen = screen;
        self.scroll = 0;
    }

    fn toggle_review_chat(&mut self) {
        self.screen = match self.screen {
            Screen::Review => Screen::Chat,
            Screen::Chat => Screen::Review,
            other => other,
        };
        self.focus = if self.screen == Screen::Chat {
            Focus::Chat
        } else {
            Focus::Diff
        };
        self.clear_visual_selection();
    }

    fn move_review_focus(&mut self, direction: char) {
        if self.screen != Screen::Review {
            self.status = "CTRL-W only moves Review windows · use Tab/gc/gr for Chat".into();
            return;
        }
        match (self.focus, direction) {
            (Focus::Diff | Focus::InlineAsk, 'h') => {
                self.picker_open = true;
                self.focus = Focus::FilePicker;
                self.status = "Focus: files → diff".into();
            }
            (Focus::FilePicker, 'l') => {
                self.focus = Focus::Diff;
                self.status = "Focus: files → diff".into();
            }
            (Focus::FilePicker, 'h') => {
                self.status = "No window to the left".into();
            }
            (Focus::Diff | Focus::InlineAsk, 'l') => {
                self.status = "No window to the right · use Tab or gc for Chat".into();
            }
            (_, 'j') => {
                self.status = "No window below".into();
            }
            (_, 'k') => {
                self.status = "No window above".into();
            }
            _ => {}
        }
        self.clear_visual_selection();
    }

    fn move_down(&mut self, amount: usize) {
        match self.focus {
            Focus::FilePicker => self.next_file_by(amount),
            Focus::Chat => {
                if self.input_mode == InputMode::Visual {
                    for _ in 0..amount {
                        self.move_chat_semantic(Movement::Down);
                    }
                } else {
                    for _ in 0..amount {
                        self.move_chat_semantic_without_scrolling(Movement::Down);
                    }
                    let max_scroll = self.chat_total_rows.saturating_sub(self.chat_viewport_rows);
                    self.chat_scroll =
                        cmp::min(max_scroll, self.chat_scroll.saturating_add(amount));
                    self.chat_autofollow = false;
                }
            }
            Focus::Diff | Focus::InlineAsk
                if self.screen == Screen::Review && self.input_mode == InputMode::Normal =>
            {
                for _ in 0..amount {
                    self.move_review_stream(StreamMovement::Down);
                }
            }
            _ => {
                let last = self.current_line_count().saturating_sub(1);
                self.cursor = cmp::min(last, self.cursor.saturating_add(amount));
                self.sync_review_visual_column();
                self.sync_review_cursor_to_current_source();
                self.ensure_cursor_visible();
            }
        }
    }

    fn move_up(&mut self, amount: usize) {
        match self.focus {
            Focus::FilePicker => {
                for _ in 0..amount {
                    self.previous_file();
                }
            }
            Focus::Chat => {
                if self.input_mode == InputMode::Visual {
                    for _ in 0..amount {
                        self.move_chat_semantic(Movement::Up);
                    }
                } else {
                    for _ in 0..amount {
                        self.move_chat_semantic_without_scrolling(Movement::Up);
                    }
                    self.chat_scroll = self.chat_scroll.saturating_sub(amount);
                    self.chat_autofollow = false;
                }
            }
            Focus::Diff | Focus::InlineAsk
                if self.screen == Screen::Review && self.input_mode == InputMode::Normal =>
            {
                for _ in 0..amount {
                    self.move_review_stream(StreamMovement::Up);
                }
            }
            _ => {
                self.cursor = self.cursor.saturating_sub(amount);
                self.sync_review_visual_column();
                self.sync_review_cursor_to_current_source();
                self.ensure_cursor_visible();
            }
        }
    }

    fn next_file_by(&mut self, amount: usize) {
        for _ in 0..amount {
            if self.current_repo_collapsed() {
                if let Some(next_repo) = (self.repo_index + 1..self.work_item.repos.len())
                    .find(|index| self.repo_is_visible(*index))
                {
                    self.repo_index = next_repo;
                    self.file_index = 0;
                }
                continue;
            }
            let current_count = self
                .current_diff()
                .map(|diff| diff.files.len())
                .unwrap_or(0);
            if self.file_index + 1 < current_count {
                self.file_index += 1;
            } else if let Some(next_repo) = (self.repo_index + 1..self.work_item.repos.len())
                .find(|index| self.repo_is_visible(*index))
            {
                self.repo_index = next_repo;
                self.file_index = 0;
            }
        }
        self.reset_file_position();
    }

    fn next_file(&mut self) {
        self.next_file_by(1);
    }

    fn previous_file(&mut self) {
        if !self.current_repo_collapsed() && self.file_index > 0 {
            self.file_index -= 1;
        } else if let Some(previous_repo) = (0..self.repo_index)
            .rev()
            .find(|index| self.repo_is_visible(*index))
        {
            self.repo_index = previous_repo;
            self.file_index = self.work_item.repos[previous_repo]
                .diff
                .files
                .len()
                .saturating_sub(1);
        }
        self.reset_file_position();
    }

    fn reset_file_position(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
        self.clear_visual_selection();
        self.sync_review_cursor_to_current_file();
    }

    fn clear_visual_selection(&mut self) {
        self.clear_review_visual_selection();
        self.chat_selection = None;
        if self.input_mode == InputMode::Visual {
            self.input_mode = InputMode::Normal;
        }
    }

    fn clear_review_visual_selection(&mut self) {
        self.visual_anchor = None;
        self.review_visual_mode = None;
        self.review_visual_anchor_column = 0;
        self.review_visual_column = 0;
        self.review_visual_preferred_column = 0;
        self.review_horizontal_scroll = 0;
    }

    fn jump_top(&mut self) {
        if self.focus == Focus::Chat {
            if !self.move_chat_semantic(Movement::Top) {
                self.chat_autofollow = false;
                self.chat_cursor = 0;
                self.chat_scroll = 0;
            }
        } else if self.input_mode == InputMode::Visual {
            self.cursor = 0;
            self.sync_review_visual_column();
            self.sync_review_cursor_to_current_source();
        } else {
            self.move_review_stream(StreamMovement::First);
        }
    }

    fn jump_bottom(&mut self) {
        if self.focus == Focus::Chat {
            if self.input_mode == InputMode::Visual {
                self.move_chat_semantic(Movement::Bottom);
            } else if self.move_chat_semantic(Movement::Bottom) {
                self.chat_autofollow = true;
            } else {
                self.chat_autofollow = true;
                self.chat_cursor = self.chat.len().saturating_sub(1);
                self.chat_scroll = self.chat_total_rows.saturating_sub(self.chat_viewport_rows);
            }
        } else if self.input_mode == InputMode::Visual {
            self.cursor = self.current_line_count().saturating_sub(1);
            self.sync_review_visual_column();
            self.sync_review_cursor_to_current_source();
        } else {
            self.move_review_stream(StreamMovement::Last);
        }
    }

    fn ensure_cursor_visible(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.viewport_height {
            self.scroll = self.cursor + 1 - self.viewport_height;
        }
    }

    fn jump_to_search(&mut self) {
        let needle = self.search.to_lowercase();
        if needle.is_empty() {
            return;
        }
        if self.focus == Focus::Chat {
            if !self.jump_to_chat_search(true, false) {
                self.status = format!("Pattern not found: {}", self.search);
            }
            return;
        }
        if self.focus == Focus::FilePicker {
            if let Some((repo, file)) = self.file_positions().into_iter().find(|(repo, file)| {
                let path = self.work_item.repos[*repo].diff.files[*file]
                    .display_path
                    .to_string_lossy()
                    .to_lowercase();
                fuzzy_contains(&path, &needle)
                    && (*repo, *file) > (self.repo_index, self.file_index)
            }) {
                self.repo_index = repo;
                self.file_index = file;
                self.reset_file_position();
            } else {
                self.status = format!("Pattern not found: {}", self.search);
            }
            return;
        }
        let matches = self.current_file().and_then(|file| {
            file.visible_lines()
                .enumerate()
                .skip(self.cursor.saturating_add(1))
                .find(|(_, line)| line.content.to_lowercase().contains(&needle))
                .map(|(index, _)| index)
        });
        if let Some(index) = matches {
            self.cursor = index;
            self.sync_review_visual_column();
            self.sync_review_cursor_to_current_source();
            self.ensure_cursor_visible();
        } else {
            self.status = format!("Pattern not found: {}", self.search);
        }
    }

    fn jump_to_search_reverse(&mut self) {
        let needle = self.search.to_lowercase();
        if needle.is_empty() {
            return;
        }
        if self.focus == Focus::Chat {
            if !self.jump_to_chat_search(false, false) {
                self.status = format!("Pattern not found: {}", self.search);
            }
            return;
        }
        if self.focus == Focus::FilePicker {
            if let Some((repo, file)) = self.file_positions().into_iter().rfind(|(repo, file)| {
                let path = self.work_item.repos[*repo].diff.files[*file]
                    .display_path
                    .to_string_lossy()
                    .to_lowercase();
                fuzzy_contains(&path, &needle)
                    && (*repo, *file) < (self.repo_index, self.file_index)
            }) {
                self.repo_index = repo;
                self.file_index = file;
                self.reset_file_position();
            } else {
                self.status = format!("Pattern not found: {}", self.search);
            }
            return;
        }
        let found = self.current_file().and_then(|file| {
            file.visible_lines()
                .enumerate()
                .take(self.cursor)
                .filter(|(_, line)| line.content.to_lowercase().contains(&needle))
                .map(|(index, _)| index)
                .last()
        });
        if let Some(index) = found {
            self.cursor = index;
            self.sync_review_visual_column();
            self.sync_review_cursor_to_current_source();
            self.ensure_cursor_visible();
        } else {
            self.status = format!("Pattern not found: {}", self.search);
        }
    }

    fn jump_to_chat_search(&mut self, forward: bool, include_current: bool) -> bool {
        if self.search.is_empty() {
            return false;
        }
        let Some(layout) = self.chat_layout.as_ref() else {
            self.status = "Chat layout is not ready yet; render once and try again".into();
            return false;
        };
        let current_message = self
            .chat_navigation
            .as_ref()
            .and_then(|cursor| {
                self.chat
                    .iter()
                    .position(|entry| entry.id == cursor.point.message_id.as_str())
                    .map(|index| (index, cursor.point.byte_offset))
            })
            .unwrap_or((self.chat_cursor.min(self.chat.len().saturating_sub(1)), 0));
        let mut found = None;
        if forward {
            for (index, entry) in self.chat.iter().enumerate().skip(current_message.0) {
                let start = if index == current_message.0 {
                    if include_current {
                        current_message.1
                    } else {
                        next_grapheme_boundary(&entry.text, current_message.1)
                    }
                } else {
                    0
                };
                if let Some(offset) = text_match_offset(&entry.text, &self.search, start, true) {
                    found = Some((index, offset));
                    break;
                }
            }
            if found.is_none() {
                for (index, entry) in self
                    .chat
                    .iter()
                    .enumerate()
                    .take(current_message.0.saturating_add(1))
                {
                    let end = if index == current_message.0 {
                        current_message.1
                    } else {
                        entry.text.len()
                    };
                    if let Some(offset) =
                        text_match_offset(&entry.text[..end], &self.search, 0, true)
                    {
                        found = Some((index, offset));
                        break;
                    }
                }
            }
        } else {
            for (index, entry) in self
                .chat
                .iter()
                .enumerate()
                .take(current_message.0.saturating_add(1))
                .rev()
            {
                let end = if index == current_message.0 {
                    current_message.1
                } else {
                    entry.text.len()
                };
                if let Some(offset) = text_match_offset(&entry.text, &self.search, end, false) {
                    found = Some((index, offset));
                    break;
                }
            }
            if found.is_none() {
                for (index, entry) in self.chat.iter().enumerate().skip(current_message.0).rev() {
                    let start = if index == current_message.0 {
                        current_message.1
                    } else {
                        0
                    };
                    let suffix = &entry.text[start..];
                    if let Some(offset) =
                        text_match_offset(suffix, &self.search, suffix.len(), false)
                    {
                        found = Some((index, start + offset));
                        break;
                    }
                }
            }
        }
        let Some((index, offset)) = found else {
            return false;
        };
        let message_id = ChatMessageId::new(self.chat[index].id.clone());
        let Some(point) = layout.point_for_message_offset(&message_id, offset) else {
            self.status = "The match is Markdown decoration and has no selectable cell".into();
            return false;
        };
        self.chat_navigation = Some(ChatCursor::new(point.clone()));
        if self.input_mode == InputMode::Visual {
            if let Some(selection) = self.chat_selection.as_mut() {
                selection.extend_to(layout, &point);
            }
        }
        self.chat_cursor = index;
        self.chat_autofollow = false;
        self.ensure_chat_navigation_visible();
        self.status = format!("Match in message {}", index + 1);
        true
    }

    fn annotation_under_cursor(&self) -> Option<&(Annotation, Placement)> {
        if self.screen == Screen::Review && self.input_mode == InputMode::Normal {
            let mut stream = self.review_stream();
            stream.set_cursor(self.review_cursor);
            if let Some(ReviewRow::Annotation { block, .. }) = stream.current() {
                return self
                    .annotations
                    .iter()
                    .find(|(annotation, _)| annotation.id == block.annotation_id);
            }
        }
        let repo = self.work_item.repos.get(self.repo_index)?;
        let file = self.current_file()?;
        self.annotations.iter().find(|(annotation, placement)| {
            let line = file
                .visible_lines()
                .nth(self.cursor)
                .and_then(|line| placement_line(line, Some(placement.side)))
                .map(|line| line as i64);
            annotation.repo_id == repo.record.id
                && annotation.file_path == file.display_path
                && line
                    .is_some_and(|line| placement.line_start <= line && line <= placement.line_end)
        })
    }

    fn file_positions(&self) -> Vec<(usize, usize)> {
        self.work_item
            .repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| !self.collapsed_repos.contains(&repo.record.id))
            .flat_map(|(repo_index, repo)| {
                (0..repo.diff.files.len()).map(move |file_index| (repo_index, file_index))
            })
            .collect()
    }

    fn select_first_file_search_match(&mut self) {
        if self.focus != Focus::FilePicker || self.search.is_empty() {
            return;
        }
        let needle = self.search.to_lowercase();
        let current_matches = self.current_file().is_some_and(|file| {
            fuzzy_contains(&file.display_path.to_string_lossy().to_lowercase(), &needle)
        });
        if current_matches {
            return;
        }
        let match_position =
            self.work_item
                .repos
                .iter()
                .enumerate()
                .find_map(|(repo_index, repo)| {
                    repo.diff
                        .files
                        .iter()
                        .position(|file| {
                            fuzzy_contains(
                                &file.display_path.to_string_lossy().to_lowercase(),
                                &needle,
                            )
                        })
                        .map(|file_index| (repo_index, file_index))
                });
        if let Some((repo_index, file_index)) = match_position {
            self.repo_index = repo_index;
            self.file_index = file_index;
            self.reset_file_position();
        }
    }

    fn current_repo_collapsed(&self) -> bool {
        self.work_item
            .repos
            .get(self.repo_index)
            .is_some_and(|repo| self.collapsed_repos.contains(&repo.record.id))
    }

    fn repo_is_visible(&self, index: usize) -> bool {
        self.work_item.repos.get(index).is_some_and(|repo| {
            !repo.diff.files.is_empty() && !self.collapsed_repos.contains(&repo.record.id)
        })
    }

    fn collapse_current_repo(&mut self) {
        if let Some(repo) = self.work_item.repos.get(self.repo_index) {
            self.collapsed_repos.insert(repo.record.id.clone());
            self.status = format!("Collapsed {}", repo.record.name);
        }
    }

    fn expand_current_repo(&mut self) {
        if let Some(repo) = self.work_item.repos.get(self.repo_index) {
            self.collapsed_repos.remove(&repo.record.id);
            self.status = format!("Expanded {}", repo.record.name);
        }
    }

    fn cursor_on_fold(&self) -> bool {
        self.current_file()
            .and_then(|file| file.visible_lines().nth(self.cursor))
            .is_some_and(|line| {
                line.kind == LineKind::Meta && line.content.contains("unchanged lines")
            })
    }

    pub(crate) fn next_context_lines(&mut self, all: bool) -> usize {
        let Some(repo) = self.work_item.repos.get(self.repo_index) else {
            return 6;
        };
        let Some(file) = repo.diff.files.get(self.file_index) else {
            return 6;
        };
        let key = (
            repo.record.id.clone(),
            file.display_path.to_string_lossy().into_owned(),
        );
        let context = if all {
            1_000_000
        } else {
            self.expanded_context
                .get(&key)
                .copied()
                .unwrap_or(6)
                .saturating_add(self.expand_step)
        };
        self.expanded_context.insert(key, context);
        context
    }

    fn jump_annotation(&mut self, forward: bool) {
        let origin = (self.repo_index, self.file_index);
        let was_visual = self.input_mode == InputMode::Visual;
        let mut stream = self.review_stream();
        stream.set_cursor(self.review_cursor);
        let target = stream.jump_annotation(forward);
        let Some(target) = target else {
            self.status = "No annotations".into();
            return;
        };
        self.review_cursor = target;
        if let Some(row) = stream.current() {
            self.sync_source_from_review_row(row);
        }
        if was_visual && origin != (self.repo_index, self.file_index) {
            self.clear_visual_selection();
            self.status = "Visual selection cleared at the file boundary".into();
        }
    }

    fn yank_current(&mut self) -> Vec<Effect> {
        if self.focus == Focus::Chat {
            let text = self
                .chat_selection
                .as_ref()
                .zip(self.chat_layout.as_ref())
                .map(|(selection, layout)| {
                    selection.copy(
                        layout,
                        CopyPolicy {
                            include_speaker_labels: true,
                        },
                    )
                })
                .unwrap_or_default();
            self.input_mode = InputMode::Normal;
            self.chat_selection = None;
            self.status = format!(
                "Copied {} byte(s) from the semantic chat selection",
                text.len()
            );
            return vec![Effect::Yank(text)];
        }
        if self.focus == Focus::FilePicker {
            let text = self
                .current_file()
                .map(|file| file.display_path.display().to_string())
                .unwrap_or_default();
            self.clear_visual_selection();
            return vec![Effect::Yank(text)];
        }
        let Some(file) = self.current_file() else {
            return Vec::new();
        };
        let (start, end) = if self.input_mode == InputMode::Visual {
            self.selection()
        } else {
            (self.cursor, self.cursor)
        };
        let selected = file
            .visible_lines()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start) + 1)
            .filter(|(_, line)| line.kind != LineKind::Meta)
            .collect::<Vec<_>>();
        let text = match self.review_selection_mode() {
            Some(ReviewSelectionMode::Line) => {
                let mut text = selected
                    .iter()
                    .map(|(_, line)| line.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    text.push('\n');
                }
                text
            }
            Some(ReviewSelectionMode::Character | ReviewSelectionMode::Block) => selected
                .iter()
                .map(|(row, line)| match self.review_row_selection(*row) {
                    Some(ReviewRowSelection::Whole) => line.content.clone(),
                    Some(ReviewRowSelection::Columns { start, end }) => {
                        source_cell_slice(&line.content, start, end)
                    }
                    None => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            None => selected
                .iter()
                .map(|(_, line)| line.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        };
        self.clear_visual_selection();
        vec![Effect::Yank(text)]
    }
}

fn source_grapheme_columns(text: &str) -> Vec<usize> {
    let mut columns = Vec::new();
    let mut column = 0usize;
    for (_, grapheme) in grapheme_indices(text) {
        columns.push(column);
        column = column.saturating_add(cell_width(grapheme).max(1));
    }
    columns
}

fn next_source_column(text: &str, current: usize) -> usize {
    let columns = source_grapheme_columns(text);
    columns
        .iter()
        .copied()
        .find(|column| *column > current)
        .unwrap_or_else(|| columns.last().copied().unwrap_or(0))
}

fn previous_source_column(text: &str, current: usize) -> usize {
    source_grapheme_columns(text)
        .into_iter()
        .take_while(|column| *column < current)
        .last()
        .unwrap_or(0)
}

fn source_column_end(text: &str, current: usize) -> usize {
    let mut column = 0usize;
    for (_, grapheme) in grapheme_indices(text) {
        let end = column.saturating_add(cell_width(grapheme).max(1));
        if column >= current || end > current {
            return end;
        }
        column = end;
    }
    column
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceWordClass {
    Space,
    Word,
    Punctuation,
}

fn source_word_columns(text: &str) -> Vec<(usize, SourceWordClass)> {
    let mut column = 0usize;
    grapheme_indices(text)
        .map(|(_, grapheme)| {
            let class = if grapheme.chars().all(char::is_whitespace) {
                SourceWordClass::Space
            } else if grapheme
                .chars()
                .all(|character| character.is_alphanumeric() || character == '_')
            {
                SourceWordClass::Word
            } else {
                SourceWordClass::Punctuation
            };
            let start = column;
            column = column.saturating_add(cell_width(grapheme).max(1));
            (start, class)
        })
        .collect()
}

fn next_source_word_column(text: &str, current: usize) -> usize {
    let columns = source_word_columns(text);
    let Some(mut index) = columns.iter().rposition(|(column, _)| *column <= current) else {
        return 0;
    };
    let class = columns[index].1;
    while index + 1 < columns.len() && columns[index + 1].1 == class {
        index += 1;
    }
    while index + 1 < columns.len() && columns[index + 1].1 == SourceWordClass::Space {
        index += 1;
    }
    columns
        .get(index + 1)
        .or_else(|| columns.last())
        .map(|(column, _)| *column)
        .unwrap_or(0)
}

fn previous_source_word_column(text: &str, current: usize) -> usize {
    let columns = source_word_columns(text);
    let Some(mut index) = columns.iter().position(|(column, _)| *column >= current) else {
        return columns.last().map(|(column, _)| *column).unwrap_or(0);
    };
    index = index.saturating_sub(1);
    while index > 0 && columns[index].1 == SourceWordClass::Space {
        index -= 1;
    }
    let class = columns[index].1;
    while index > 0 && columns[index - 1].1 == class {
        index -= 1;
    }
    columns[index].0
}

fn source_cell_slice(text: &str, start: usize, end: usize) -> String {
    let mut output = String::new();
    let mut column = 0usize;
    for (_, grapheme) in grapheme_indices(text) {
        let width = cell_width(grapheme).max(1);
        let grapheme_end = column.saturating_add(width).saturating_sub(1);
        if column <= end && grapheme_end >= start {
            output.push_str(grapheme);
        }
        column = column.saturating_add(width);
        if column > end {
            break;
        }
    }
    output
}

fn terminal_enter(key: &KeyEvent) -> bool {
    (key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT))
        || (matches!(key.code, KeyCode::Char('j' | 'm'))
            && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn editor_cursor_positions(
    text: &str,
    width: usize,
    initial_column: usize,
) -> Vec<(usize, usize, usize)> {
    let width = width.max(1);
    let mut positions = Vec::with_capacity(text.len().saturating_add(1));
    let mut row = 0usize;
    let mut column = initial_column.min(width.saturating_sub(1));
    for (byte, grapheme) in grapheme_indices(text) {
        positions.push((byte, row, column));
        if grapheme == "\n" {
            row = row.saturating_add(1);
            column = 0;
            continue;
        }
        let grapheme_width = if grapheme == "\t" {
            1
        } else {
            cell_width(grapheme)
        };
        if column > 0 && column.saturating_add(grapheme_width) > width {
            row = row.saturating_add(1);
            column = 0;
            if let Some(position) = positions.last_mut() {
                position.1 = row;
                position.2 = column;
            }
        }
        column = column.saturating_add(grapheme_width);
    }
    positions.push((text.len(), row, column));
    positions
}

fn placement_line(line: &crate::diff::DiffLine, side: Option<AnchorSide>) -> Option<usize> {
    match side.unwrap_or_default() {
        AnchorSide::Old => line.old_line,
        AnchorSide::New => line.new_line,
    }
}

fn fuzzy_contains(haystack: &str, needle: &str) -> bool {
    let mut characters = needle.chars();
    let mut expected = characters.next();
    for character in haystack.chars() {
        if expected == Some(character) {
            expected = characters.next();
            if expected.is_none() {
                return true;
            }
        }
    }
    expected.is_none()
}

fn text_match_offset(text: &str, needle: &str, boundary: usize, forward: bool) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let boundary = floor_grapheme_boundary(text, boundary.min(text.len()));
    if needle.is_ascii() {
        let needle = needle.as_bytes();
        let bytes = text.as_bytes();
        if forward {
            return bytes[boundary..]
                .windows(needle.len())
                .position(|candidate| candidate.eq_ignore_ascii_case(needle))
                .map(|offset| boundary + offset);
        }
        return bytes[..boundary]
            .windows(needle.len())
            .rposition(|candidate| candidate.eq_ignore_ascii_case(needle));
    }
    if forward {
        text[boundary..]
            .find(needle)
            .map(|offset| boundary + offset)
    } else {
        text[..boundary].rfind(needle)
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::PathBuf;

    use super::AppState;
    use crate::diff::{DiffFile, DiffLine, DiffSet, FileStatus, Hunk, LineKind};
    use crate::domain::{BaseBranchSource, Repo, Version, VersionKind, WorkItem};
    use crate::work_item::{ResolvedWorkItem, ReviewRepo};

    pub(crate) fn state_for_ui() -> AppState {
        let file = DiffFile {
            old_path: Some("a.rs".into()),
            new_path: Some("a.rs".into()),
            display_path: "a.rs".into(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1 +1,2 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        old_line: Some(1),
                        new_line: Some(1),
                        content: "one".into(),
                    },
                    DiffLine {
                        kind: LineKind::Addition,
                        old_line: None,
                        new_line: Some(2),
                        content: "two".into(),
                    },
                ],
            }],
        };
        AppState::new(ResolvedWorkItem {
            item: WorkItem {
                id: "w".into(),
                name: "demo".into(),
                workspace_root: PathBuf::from("/demo"),
                created_at: String::new(),
                updated_at: String::new(),
                last_opened_at: None,
            },
            repos: vec![ReviewRepo {
                record: Repo {
                    id: "r".into(),
                    work_item_id: "w".into(),
                    name: "repo".into(),
                    path: PathBuf::from("/demo"),
                    remote_pr_url: None,
                    pr_meta_json: None,
                    base_branch: Some("main".into()),
                    base_branch_source: BaseBranchSource::Auto,
                    last_activity_at: None,
                },
                version: Version {
                    id: "v".into(),
                    repo_id: "r".into(),
                    version_num: 0,
                    kind: VersionKind::WorkingTree,
                    created_at: String::new(),
                    head_sha: "abc".into(),
                    worktree_path: None,
                    last_opened_at: None,
                },
                diff: DiffSet { files: vec![file] },
            }],
            session_root: PathBuf::from("/session"),
        })
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::tests_support::state_for_ui;
    use super::{
        AgentPhase, AppState, ComposeTarget, DiffLayout, Effect, Focus, InputMode, MarkdownPreview,
        PruneChoice, Screen,
    };
    use crate::diff::{DiffLine, Hunk, LineKind};
    use crate::domain::{AskMessage, DeliveryState};

    fn state() -> AppState {
        state_for_ui()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q_does_not_quit_top_level_but_command_does() {
        let mut app = state();
        assert!(app.handle_key(key(KeyCode::Char('q'))).is_empty());
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Char(':')));
        app.handle_key(key(KeyCode::Char('q')));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::Quit { force: false }]);
    }

    #[test]
    fn command_toggles_diff_layout() {
        let mut app = state();
        app.input_mode = InputMode::Command;
        for character in "diff unified".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.layout, DiffLayout::Unified);
    }

    #[test]
    fn command_completion_matches_subcommands_and_executes_the_selected_literal() {
        let matches = super::command_matches("diff u");
        assert_eq!(matches, vec![("diff unified", "show a single-column diff")]);

        let mut app = state();
        app.input_mode = InputMode::Command;
        for character in "diff u".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Vec::new());
        assert_eq!(app.layout, DiffLayout::Unified);
        assert!(!app.status.contains("Unknown command"));
    }

    #[test]
    fn colon_side_and_main_enter_the_chat_surface_before_the_effect_runs() {
        let mut side = state();
        side.input_mode = InputMode::Command;
        for character in "side inspect this".chars() {
            side.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            side.handle_key(key(KeyCode::Enter)),
            vec![Effect::StartSide(Some("inspect this".into()))]
        );
        assert_eq!(side.screen, Screen::Chat);
        assert_eq!(side.focus, Focus::Chat);
        assert_eq!(side.input_mode, InputMode::Normal);
        assert_eq!(side.compose_target, Some(ComposeTarget::Chat));

        let mut main = state();
        main.side_active = true;
        main.input_mode = InputMode::Command;
        for character in "main".chars() {
            main.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(main.handle_key(key(KeyCode::Enter)), vec![Effect::ExitSide]);
        assert_eq!(main.screen, Screen::Chat);
        assert_eq!(main.focus, Focus::Chat);
        assert_eq!(main.input_mode, InputMode::Normal);
        assert_eq!(main.compose_target, Some(ComposeTarget::Chat));
    }

    #[test]
    fn model_command_always_uses_the_runtime_picker_and_rejects_arbitrary_names() {
        assert_eq!(
            super::command_matches("model"),
            vec![("model", "pick a runtime-supported model in stages")]
        );
        assert!(
            super::command_matches("model definitely-not-runtime-advertised").is_empty()
        );

        let mut app = state();
        app.input_mode = InputMode::Command;
        for character in "model definitely-not-runtime-advertised".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.status.contains("Unknown command"));
        assert!(app
            .model_options
            .iter()
            .all(|option| option.id != "definitely-not-runtime-advertised"));
    }

    #[test]
    fn command_aliases_are_discoverable_when_the_reducer_accepts_them() {
        for command in ["quit", "abort", "progress", "side-exit"] {
            assert!(
                !super::command_matches(command).is_empty(),
                "missing palette descriptor for :{command}"
            );
        }
    }

    #[test]
    fn fold_rows_map_to_their_source_line_for_context_expansion() {
        let mut app = state();
        app.work_item.repos[0].diff.files[0].hunks[0].lines = vec![
            DiffLine {
                kind: LineKind::Context,
                old_line: Some(1),
                new_line: Some(1),
                content: "visible before fold".into(),
            },
            DiffLine {
                kind: LineKind::Meta,
                old_line: None,
                new_line: None,
                content: "⋯ 6 unchanged lines".into(),
            },
        ];
        app.sync_review_cursor_to_current_file();
        app.screen = Screen::Review;
        app.focus = Focus::Diff;
        app.input_mode = InputMode::Normal;

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.cursor, 1, "the fold must map after the prior hunk line");
        assert_eq!(
            app.handle_key(key(KeyCode::Char('o'))),
            vec![Effect::ExpandContext { all: false }]
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('O'))),
            vec![Effect::ExpandContext { all: true }]
        );
    }

    #[test]
    fn command_palette_wraps_and_supports_home_and_end() {
        let mut app = state();
        app.input_mode = InputMode::Command;
        app.command_viewport_rows = 4;
        let count = super::command_matches("").len();

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command_index, count - 1);
        assert_eq!(app.command_scroll, count - 4);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command_index, 0);
        assert_eq!(app.command_scroll, 0);

        app.handle_key(key(KeyCode::End));
        assert_eq!(app.command_index, count - 1);
        assert_eq!(app.command_scroll, count - 4);

        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.command_index, 0);
        assert_eq!(app.command_scroll, 0);
    }

    #[test]
    fn prune_screen_marks_the_open_work_item_as_unselectable() {
        let mut app = state();
        app.screen = Screen::Prune;
        app.prune_items = vec![PruneChoice {
            id: app.work_item.item.id.clone(),
            name: "open".into(),
            last_opened_at: "now".into(),
            versions: 1,
            annotations: 0,
            selected: false,
        }];

        assert!(app.handle_key(key(KeyCode::Char(' '))).is_empty());
        assert!(!app.prune_items[0].selected);
        assert!(app.status.contains("open Work Item cannot be selected"));
    }

    #[test]
    fn composer_up_and_down_follow_wrapped_visual_rows() {
        let mut app = state();
        app.screen = Screen::Chat;
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(ComposeTarget::Chat);
        app.compose_wrap_width = 8;
        app.compose = "first wrapped visual row".into();
        app.compose_cursor = app.compose.len();

        app.handle_key(key(KeyCode::Up));
        let upper = app.compose_cursor;
        assert!(upper < app.compose.len());
        app.handle_key(key(KeyCode::Down));
        assert!(app.compose_cursor > upper);
    }

    #[test]
    fn contextual_composer_navigation_accounts_for_the_prompt_prefix() {
        let mut app = state();
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(ComposeTarget::Annotation(
            crate::domain::AnnotationKind::Ask,
        ));
        app.compose_wrap_width = 44;
        app.compose = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQR".into();
        app.compose_cursor = app.compose.len();

        app.handle_key(key(KeyCode::Up));

        assert!(app.compose_cursor < app.compose.len());
        assert_eq!(
            super::editor_cursor_positions(&app.compose, 44, 2)
                .into_iter()
                .find(|(byte, _, _)| *byte == app.compose_cursor)
                .map(|(_, row, _)| row),
            Some(0)
        );
    }

    #[test]
    fn composer_combining_marks_do_not_consume_terminal_columns() {
        let positions = super::editor_cursor_positions("e\u{301}x", 1, 0);
        assert_eq!(
            positions
                .iter()
                .find(|(byte, _, _)| *byte == "e\u{301}".len())
                .map(|(_, row, column)| (*row, *column)),
            Some((1, 0))
        );
        assert_eq!(
            positions.last().map(|(_, row, column)| (*row, *column)),
            Some((1, 1))
        );
    }

    #[test]
    fn composer_edits_extended_graphemes_atomically() {
        let mut app = state();
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(ComposeTarget::Chat);

        app.compose = "x👩\u{200d}💻y".into();
        app.compose_cursor = "x👩\u{200d}💻".len();
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.compose, "xy");
        assert_eq!(app.compose_cursor, 1);

        app.compose = "x🇨🇦y".into();
        app.compose_cursor = 1;
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.compose, "xy");
        assert_eq!(app.compose_cursor, 1);

        app.compose = "x1\u{fe0f}\u{20e3}y".into();
        app.compose_cursor = app.compose.len();
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.compose_cursor, 1);
    }

    #[test]
    fn ctrl_c_stops_active_response_before_discarding_chat_draft() {
        let mut app = state();
        app.screen = Screen::Chat;
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(ComposeTarget::Chat);
        app.compose = "keep this draft".into();
        app.compose_cursor = app.compose.len();
        app.agent_progress.phase = AgentPhase::Responding;
        app.agent_progress.active_outbound_id = Some("active".into());

        let effects = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(effects, vec![Effect::AbortAgent]);
        assert_eq!(app.compose, "keep this draft");
        assert_eq!(app.agent_progress.phase, AgentPhase::Stopping);
    }

    #[test]
    fn steer_command_interrupts_only_when_a_response_is_active() {
        let mut app = state();
        app.screen = Screen::Chat;
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(ComposeTarget::Chat);
        app.compose = "/steer focus on the error path".into();
        app.compose_cursor = app.compose.len();
        app.agent_progress.phase = AgentPhase::Responding;
        app.agent_progress.active_outbound_id = Some("active".into());

        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::SteerChat("focus on the error path".into())]
        );

        app.input_mode = InputMode::Compose;
        app.compose_target = Some(ComposeTarget::Chat);
        app.compose = "/steer now answer normally".into();
        app.compose_cursor = app.compose.len();
        app.agent_progress.phase = AgentPhase::Idle;
        app.agent_progress.active_outbound_id = None;
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![Effect::SendChat("now answer normally".into())]
        );
    }

    #[test]
    fn tab_toggles_review_and_chat_only_in_normal_mode() {
        let mut app = state();
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Chat);
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(super::ComposeTarget::Annotation(
            crate::domain::AnnotationKind::Comment,
        ));
        app.handle_key(key(KeyCode::Tab));
        assert!(app.compose.ends_with("    "));
        assert_eq!(app.screen, Screen::Chat);
    }

    #[test]
    fn vim_prefixes_toggle_modes_and_preview() {
        let mut app = state();
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.screen, Screen::Chat);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.screen, Screen::Chat);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(app.screen, Screen::Review);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(app.screen, Screen::Review);
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('m'))),
            vec![Effect::Preview]
        );
    }

    #[test]
    fn markdown_preview_restores_the_exact_prior_chat_draft() {
        let mut app = state();
        app.screen = Screen::Chat;
        app.focus = Focus::Chat;
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(ComposeTarget::Chat);
        app.compose = "draft with **markdown**".into();
        app.compose_cursor = 7;
        app.compose_scroll = 2;
        app.status = "draft status".into();

        app.open_preview("# Preview\n\nbody".into());
        assert_eq!(app.screen, Screen::Preview);
        assert_eq!(app.preview_scroll, 0);
        app.preview_total_rows = 40;
        app.viewport_height = 8;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.preview_scroll > 0);
        app.handle_key(key(KeyCode::Esc));

        assert_eq!(app.screen, Screen::Chat);
        assert_eq!(app.focus, Focus::Chat);
        assert_eq!(app.input_mode, InputMode::Compose);
        assert_eq!(app.compose, "draft with **markdown**");
        assert_eq!(app.compose_cursor, 7);
        assert_eq!(app.compose_scroll, 2);
        assert_eq!(app.compose_target, Some(ComposeTarget::Chat));
        assert_eq!(app.status, "draft status");
        assert!(app.preview_markdown.is_none());
    }

    #[test]
    fn markdown_preview_supports_vim_and_page_scroll_commands() {
        let mut app = state();
        app.open_preview((0..80).map(|n| format!("line {n}\n")).collect());
        app.preview_total_rows = 80;
        app.viewport_height = 10;

        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.preview_scroll, 70);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.preview_scroll, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(app.preview_scroll, 10);
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn recovery_requires_an_explicit_resend_or_discard() {
        let mut app = state();
        app.screen = Screen::Recovery;
        app.pending_asks.push(AskMessage {
            id: "message".into(),
            annotation_id: "annotation".into(),
            seq: 0,
            role: "user".into(),
            text: "was this sent?".into(),
            sent: false,
            delivery_state: DeliveryState::Pending,
            ts: String::new(),
        });
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('r'))).as_slice(),
            [Effect::ResendPendingAsk(_)]
        ));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('d'))),
            vec![Effect::DiscardPendingAsk("message".into())]
        );
    }

    #[test]
    fn picker_repo_groups_collapse_and_expand_with_h_and_l() {
        let mut app = state();
        app.focus = Focus::FilePicker;
        app.handle_key(key(KeyCode::Char('h')));
        assert!(app.collapsed_repos.contains("r"));
        app.handle_key(key(KeyCode::Char('l')));
        assert!(!app.collapsed_repos.contains("r"));
    }

    #[test]
    fn picker_enter_accepts_the_current_file_and_returns_focus_to_diff() {
        let mut app = state();
        app.picker_open = true;
        app.focus = Focus::FilePicker;
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.picker_open);
        assert_eq!(app.focus, Focus::Diff);
    }

    #[test]
    fn context_expansion_increases_by_the_configured_step() {
        let mut app = state();
        assert_eq!(app.next_context_lines(false), 16);
        assert_eq!(app.next_context_lines(false), 26);
        assert_eq!(app.next_context_lines(true), 1_000_000);
    }

    #[test]
    fn settings_screen_edits_the_expansion_step() {
        let mut app = state();
        app.screen = Screen::Settings;
        app.settings_index = 7;
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.input_mode, InputMode::Compose);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            vec![Effect::SetExpandStep(10)]
        );
    }

    #[test]
    fn settings_screen_exposes_persistent_launch_preferences() {
        let mut app = state();
        app.screen = Screen::Settings;

        app.settings_index = 2;
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![Effect::SetDefaultDiffLayout(DiffLayout::Split)]
        );

        app.settings_index = 8;
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![Effect::SetFileTreeDefault(false)]
        );

        app.settings_index = 9;
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![Effect::SetMarkdownPreview(MarkdownPreview::Browser)]
        );
    }

    #[test]
    fn markdown_default_controls_gm_and_inline_preview_can_open_browser() {
        let mut app = state();
        app.markdown_preview = MarkdownPreview::Browser;
        app.pending_prefix = "g".into();
        assert_eq!(
            app.handle_key(key(KeyCode::Char('m'))),
            vec![Effect::PreviewBrowser]
        );

        app.open_preview("# Result".into());
        assert_eq!(
            app.handle_key(key(KeyCode::Char('o'))),
            vec![Effect::PreviewBrowser]
        );
        assert_eq!(app.screen, Screen::Preview);
    }

    #[test]
    fn stop_command_aborts_the_active_agent_turn() {
        let mut app = state();
        app.agent_progress.phase = AgentPhase::Responding;
        app.agent_progress.active_outbound_id = Some("active".into());
        app.input_mode = InputMode::Command;
        for character in "stop".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![Effect::AbortAgent]
        );
        assert_eq!(app.agent_progress.phase, AgentPhase::Stopping);
        assert!(app.status.contains("Stopping the current Copilot response"));

        app.input_mode = InputMode::Command;
        for character in "stop".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.status.contains("already in progress"));
    }

    #[test]
    fn queue_and_agent_status_stop_controls_share_the_same_transition() {
        for screen in [Screen::Queue, Screen::AgentStatus] {
            let mut app = state();
            app.screen = screen;
            app.agent_progress.phase = AgentPhase::Tool;
            app.agent_progress.active_outbound_id = Some("active".into());

            assert_eq!(
                app.handle_key(key(KeyCode::Char('s'))),
                vec![Effect::AbortAgent]
            );
            assert_eq!(app.agent_progress.phase, AgentPhase::Stopping);
            assert_eq!(
                app.agent_progress.active_outbound_id.as_deref(),
                Some("active")
            );
        }

        let mut diagnostics = state();
        diagnostics.screen = Screen::AgentStatus;
        diagnostics.agent_progress.phase = AgentPhase::Responding;
        diagnostics.agent_progress.active_outbound_id = Some("active".into());
        assert_eq!(
            diagnostics.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,)),
            vec![Effect::AbortAgent]
        );
        assert_eq!(diagnostics.agent_progress.phase, AgentPhase::Stopping);

        let mut idle = state();
        idle.input_mode = InputMode::Command;
        for character in "abort".chars() {
            idle.handle_key(key(KeyCode::Char(character)));
        }
        assert!(idle.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(idle.status.contains("No active Copilot response"));
    }

    #[test]
    fn ctrl_s_submits_chat_in_terminals_without_modified_enter() {
        let mut app = state();
        app.screen = Screen::Chat;
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(super::ComposeTarget::Chat);
        app.compose = "hello".into();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            vec![Effect::SendChat("hello".into())]
        );
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn ctrl_w_then_plain_direction_moves_focus_as_documented() {
        let mut app = state();
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(app.status.contains("CTRL-W"));
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.focus, Focus::FilePicker);
        assert!(app.picker_open);

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.focus, Focus::FilePicker);
        assert!(app.status.contains("No window below"));

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.focus, Focus::Diff);

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.focus, Focus::Diff);
        assert!(app.status.contains("No window to the right"));

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.pending_prefix_started =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        app.tick(std::time::Instant::now());
        assert!(app.pending_prefix.is_empty());
        assert!(app.status.contains("timed out"));
    }

    #[test]
    fn generic_key_prefixes_are_visible_cancellable_and_time_out() {
        let mut app = state();
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.pending_prefix, "g");
        assert!(app.pending_prefix_started.is_some());
        assert!(app.status.contains("waiting for g/c/r/m"));

        app.handle_key(key(KeyCode::Esc));
        assert!(app.pending_prefix.is_empty());
        assert!(app.status.contains("KEY g cancelled"));

        app.handle_key(key(KeyCode::Char(']')));
        app.pending_prefix_started =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        app.tick(std::time::Instant::now());
        assert!(app.pending_prefix.is_empty());
        assert!(app.status.contains("KEY ] timed out"));

        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.pending_prefix.is_empty());
        assert!(app.status.contains("Unknown key chord: gx"));
    }

    #[test]
    fn t_is_the_only_direct_file_tree_toggle() {
        let mut app = state();
        assert!(!app.picker_open);
        app.handle_key(key(KeyCode::Char('t')));
        assert!(app.picker_open);
        assert_eq!(app.focus, Focus::FilePicker);
        app.handle_key(key(KeyCode::Char('t')));
        assert!(!app.picker_open);
        assert_eq!(app.focus, Focus::Diff);

        app.handle_key(key(KeyCode::Char('-')));
        assert!(!app.picker_open);
    }

    #[test]
    fn terminal_ctrl_j_equivalent_submits_composers_and_commands() {
        let mut app = state();
        app.input_mode = InputMode::Compose;
        app.compose_target = Some(super::ComposeTarget::Chat);
        app.compose = "hello".into();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            vec![Effect::SendChat("hello".into())]
        );

        app.input_mode = InputMode::Command;
        app.command = "q!".into();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            vec![Effect::Quit { force: true }]
        );
    }
}

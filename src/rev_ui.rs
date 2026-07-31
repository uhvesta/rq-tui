use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend, TestBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use uuid::Uuid;

use crate::annotations::{anchor_from_diff, create_local_annotation, AnnotationRequest};
use crate::chat_render::render_markdown_mapped;
use crate::cmux::MarkdownSurface;
use crate::config::AppPaths;
use crate::copilot::{
    start_agent, start_model_catalog, AgentCommand, AgentEvent, AgentRuntime, BridgeConfig,
    ModelOption, ModelSelection, Outbound, OutboundKind,
};
use crate::diff::{DiffFile, DiffLine, FileStatus, Hunk, LineKind};
use crate::domain::{
    AnchorSide, Annotation, AnnotationKind, AnnotationStatus, AskMessage, BaseBranchSource,
    DeliveryState, Placement,
};
use crate::export::CommentExport;
use crate::git::Git;
use crate::highlight::{Highlighter, PlainHighlighter, StyledSegment, SyntectHighlighter};
use crate::markdown_preview::MarkdownPreviewServer;
use crate::remote::{PrReference, RemoteResolver};
use crate::storage::{now, RevQuestionSession, Storage};
use crate::terminal_text::{
    cell_width, floor_grapheme_boundary, grapheme_indices, next_grapheme_boundary,
    previous_grapheme_boundary,
};
use crate::ui::{copy_to_clipboard, ClipboardDelivery};
use crate::work_item::{resolve_local, ResolvedWorkItem, ReviewRepo};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_EVENTS_PER_TICK: usize = 128;
const REV_DEFAULT_MODEL_SETTING: &str = "rev.model.default.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevMode {
    Normal,
    Visual,
    Compose,
    Command,
    Model,
    History,
    FilePicker,
    Questions,
    Help,
    ConfirmClear,
    ConfirmResolve,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RevDiffLayout {
    #[default]
    Unified,
    Split,
}

impl RevDiffLayout {
    fn label(self) -> &'static str {
        match self {
            Self::Unified => "unified",
            Self::Split => "split",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CommandPalette {
    input: String,
    cursor: usize,
    selected: usize,
}

#[derive(Clone, Debug)]
enum ComposeTarget {
    Feedback,
    EditFeedback(String),
    NewQuestion,
    FollowUp(String),
}

enum ContextualAnnotationMatch {
    None,
    Match(String),
    Blocked(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerStage {
    Model,
    Reasoning,
    Context,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PickerScope {
    Thread,
    GlobalDefault,
}

#[derive(Clone, Debug)]
struct ModelPicker {
    models: Vec<ModelOption>,
    initial_selection: ModelSelection,
    stage: PickerStage,
    index: usize,
    model_index: usize,
    reasoning_effort: Option<String>,
    query: String,
    scope: PickerScope,
}

impl ModelPicker {
    fn new(
        models: Vec<ModelOption>,
        initial_selection: ModelSelection,
        scope: PickerScope,
    ) -> Self {
        let model_index = models
            .iter()
            .position(|model| model.id == initial_selection.model_id)
            .unwrap_or(0);
        Self {
            models,
            initial_selection,
            stage: PickerStage::Model,
            index: model_index,
            model_index,
            reasoning_effort: None,
            query: String::new(),
            scope,
        }
    }

    fn filtered_model_indices(&self) -> Vec<usize> {
        let query = self.query.trim().to_ascii_lowercase();
        self.models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                query.is_empty()
                    || model.id.to_ascii_lowercase().contains(&query)
                    || model.name.to_ascii_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn model(&self) -> &ModelOption {
        &self.models[self.model_index.min(self.models.len().saturating_sub(1))]
    }

    fn option_count(&self) -> usize {
        match self.stage {
            PickerStage::Model => self.filtered_model_indices().len(),
            PickerStage::Reasoning => self.model().supported_reasoning_efforts.len() + 1,
            PickerStage::Context => self.model().context_tiers.len() + 1,
        }
    }

    fn selection(&self) -> ModelSelection {
        let model = self.model();
        let context_tier = match self.stage {
            PickerStage::Context => self
                .index
                .checked_sub(1)
                .and_then(|index| model.context_tiers.get(index))
                .map(|tier| tier.id.clone()),
            _ => None,
        };
        ModelSelection {
            model_id: model.id.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            context_tier,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingSend {
    annotation_id: String,
    user_message_id: String,
    assistant_message_id: String,
    assistant_seq: i64,
    prompt: String,
    needs_model_picker: bool,
}

#[derive(Clone, Debug)]
struct QuestionLaunch {
    annotation_id: String,
    pending: Option<PendingSend>,
    existing: Option<RevQuestionSession>,
}

struct RevAgentSlot {
    question_id: String,
    runtime: Box<dyn AgentRuntime>,
    pending: Option<PendingSend>,
    outbound_id: Option<String>,
    session_id: Option<String>,
    selection: ModelSelection,
    needs_model_picker: bool,
    model_switch_only: bool,
}

#[derive(Clone)]
enum RevRowKind {
    Source {
        visible_index: usize,
        line: DiffLine,
    },
    Annotation {
        annotation_id: String,
        anchor_visible_index: usize,
    },
}

#[derive(Clone)]
struct RevRow {
    kind: RevRowKind,
    line: Option<Line<'static>>,
    yank_text: String,
    item_text: String,
}

struct RenderCache {
    revision: u64,
    width: usize,
    rows: Vec<RevRow>,
}

#[derive(Clone, Copy)]
enum FilePickerRow {
    Repo(usize),
    File {
        repo_index: usize,
        flat_index: usize,
    },
}

struct RevState {
    workspace: ResolvedWorkItem,
    files: Vec<(usize, usize)>,
    file_index: usize,
    row_cursor: usize,
    review_scroll: usize,
    review_viewport_height: usize,
    terminal_height: usize,
    file_row_cursors: HashMap<usize, usize>,
    file_review_scrolls: HashMap<usize, usize>,
    visual_anchor: Option<usize>,
    visual_row_anchor: Option<usize>,
    pending_yank: bool,
    mode: RevMode,
    diff_layout: RevDiffLayout,
    command: CommandPalette,
    branch_candidates: Vec<String>,
    original_context: HashSet<String>,
    original_files: HashMap<String, DiffFile>,
    show_comments: bool,
    show_questions: bool,
    compose_target: Option<ComposeTarget>,
    compose: String,
    compose_cursor: usize,
    compose_scroll: u16,
    help_scroll: u16,
    file_picker_cursor: usize,
    file_picker_return_mode: RevMode,
    file_tree_open: bool,
    collapsed_repos: HashSet<usize>,
    question_cursor: usize,
    question_return_mode: RevMode,
    questions_open: bool,
    model_return_mode: RevMode,
    status: String,
    annotations: Vec<(Annotation, Placement)>,
    threads: HashMap<String, Vec<AskMessage>>,
    question_sessions: HashMap<String, RevQuestionSession>,
    picker: Option<ModelPicker>,
    model_catalog: Option<Receiver<Result<Vec<ModelOption>>>>,
    model_catalog_started: Option<Instant>,
    default_model: Option<ModelSelection>,
    agent: Option<RevAgentSlot>,
    queued_questions: VecDeque<QuestionLaunch>,
    streaming: bool,
    agent_activity: String,
    agent_last_event: Instant,
    rows_revision: u64,
    render_cache: Option<RenderCache>,
    history_cursor: usize,
    history_delete_armed: Option<String>,
    pending_resolve: Option<String>,
    refresh: Option<Receiver<Result<ResolvedWorkItem>>>,
    refresh_started: Option<Instant>,
    markdown_preview: bool,
    cmux_markdown_requested: bool,
    cmux_markdown: Option<MarkdownSurface>,
    cmux_open: Option<Receiver<Result<Option<MarkdownSurface>>>>,
    cmux_close: Option<Receiver<()>>,
    markdown_server: Option<MarkdownPreviewServer>,
    markdown_sync_key: Option<String>,
    markdown_content_key: Option<String>,
    markdown_sides_cache: Option<MarkdownSides>,
}

impl RevState {
    fn load(workspace: ResolvedWorkItem, storage: &Storage) -> Result<Self> {
        let files = workspace
            .repos
            .iter()
            .enumerate()
            .flat_map(|(repo_index, repo)| {
                (0..repo.diff.files.len()).map(move |file_index| (repo_index, file_index))
            })
            .collect::<Vec<_>>();
        let mut annotations = Vec::new();
        let mut threads = HashMap::new();
        let mut question_sessions = HashMap::new();
        for repo in &workspace.repos {
            for pair in storage.annotation_history_for_version(&repo.version.id)? {
                let annotation = &pair.0;
                if annotation.kind == AnnotationKind::Ask {
                    threads.insert(
                        annotation.id.clone(),
                        storage.ask_messages_for_annotation(&annotation.id)?,
                    );
                    if let Some(session) = storage.rev_question_session(&annotation.id)? {
                        question_sessions.insert(annotation.id.clone(), session);
                    }
                }
                annotations.push(pair);
            }
        }
        let branch_candidates = workspace
            .repos
            .iter()
            .flat_map(|repo| {
                Git::default()
                    .branch_candidates(&repo.record.path)
                    .unwrap_or_default()
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let original_context = context_keys(&workspace);
        let original_files = workspace
            .repos
            .iter()
            .flat_map(|repo| {
                repo.diff
                    .files
                    .iter()
                    .map(move |file| (review_file_key(&repo.record.id, file.path()), file.clone()))
            })
            .collect();
        Ok(Self {
            workspace,
            files,
            file_index: 0,
            row_cursor: 0,
            review_scroll: 0,
            review_viewport_height: 1,
            terminal_height: 9,
            file_row_cursors: HashMap::new(),
            file_review_scrolls: HashMap::new(),
            visual_anchor: None,
            visual_row_anchor: None,
            pending_yank: false,
            mode: RevMode::Normal,
            diff_layout: RevDiffLayout::Unified,
            command: CommandPalette::default(),
            branch_candidates,
            original_context,
            original_files,
            show_comments: true,
            show_questions: true,
            compose_target: None,
            compose: String::new(),
            compose_cursor: 0,
            compose_scroll: 0,
            help_scroll: 0,
            file_picker_cursor: 0,
            file_picker_return_mode: RevMode::Normal,
            file_tree_open: false,
            collapsed_repos: HashSet::new(),
            question_cursor: 0,
            question_return_mode: RevMode::Normal,
            questions_open: false,
            model_return_mode: RevMode::Normal,
            status: "j/k stay in this file · h/l change files".into(),
            annotations,
            threads,
            question_sessions,
            picker: None,
            model_catalog: None,
            model_catalog_started: None,
            default_model: storage
                .setting(REV_DEFAULT_MODEL_SETTING)?
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .context("saved rev default model is invalid")?,
            agent: None,
            queued_questions: VecDeque::new(),
            streaming: false,
            agent_activity: "Copilot starts only when you ask a question".into(),
            agent_last_event: Instant::now(),
            rows_revision: 1,
            render_cache: None,
            history_cursor: 0,
            history_delete_armed: None,
            pending_resolve: None,
            refresh: None,
            refresh_started: None,
            markdown_preview: false,
            cmux_markdown_requested: false,
            cmux_markdown: None,
            cmux_open: None,
            cmux_close: None,
            markdown_server: None,
            markdown_sync_key: None,
            markdown_content_key: None,
            markdown_sides_cache: None,
        })
    }

    fn current_repo(&self) -> Option<&ReviewRepo> {
        let (repo, _) = *self.files.get(self.file_index)?;
        self.workspace.repos.get(repo)
    }

    fn current_file(&self) -> Option<&DiffFile> {
        let (repo, file) = *self.files.get(self.file_index)?;
        self.workspace
            .repos
            .get(repo)
            .and_then(|repo| repo.diff.files.get(file))
    }

    fn current_indices(&self) -> Option<(usize, usize)> {
        self.files.get(self.file_index).copied()
    }

    fn invalidate_rows(&mut self) {
        self.rows_revision = self.rows_revision.wrapping_add(1).max(1);
        self.render_cache = None;
    }

    fn clamp_cursor(&mut self, row_count: usize) {
        self.row_cursor = self.row_cursor.min(row_count.saturating_sub(1));
    }

    fn switch_file(&mut self, target: usize) -> bool {
        if target >= self.files.len() || target == self.file_index {
            return false;
        }
        self.file_row_cursors
            .insert(self.file_index, self.row_cursor);
        self.file_review_scrolls
            .insert(self.file_index, self.review_scroll);
        self.file_index = target;
        self.row_cursor = self.file_row_cursors.get(&target).copied().unwrap_or(0);
        self.review_scroll = self.file_review_scrolls.get(&target).copied().unwrap_or(0);
        self.visual_anchor = None;
        self.visual_row_anchor = None;
        self.pending_yank = false;
        self.render_cache = None;
        sync_file_picker_cursor(self);
        true
    }

    fn move_file(&mut self, forward: bool) {
        if self.files.is_empty() {
            return;
        }
        let target = if forward {
            (self.file_index + 1).min(self.files.len() - 1)
        } else {
            self.file_index.saturating_sub(1)
        };
        if !self.switch_file(target) {
            self.status = if forward {
                "Already at the last file".into()
            } else {
                "Already at the first file".into()
            };
            return;
        }
        self.mode = RevMode::Normal;
        self.status = "Changed file with h/l · restored this file's previous position".into();
    }

    fn annotation(&self, id: &str) -> Option<&(Annotation, Placement)> {
        self.annotations
            .iter()
            .find(|(annotation, _)| annotation.id == id)
    }
}

pub(crate) fn run(workspace: ResolvedWorkItem, storage: &Storage, paths: &AppPaths) -> Result<()> {
    let mut state = RevState::load(workspace, storage)?;
    recover_pending_questions(&mut state, storage, paths)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut highlighter = SyntectHighlighter::default();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|panic| {
        restore_terminal();
        eprintln!("{panic}");
    }));
    let result = run_loop(&mut terminal, &mut state, storage, paths, &mut highlighter);
    std::panic::set_hook(previous_hook);
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
    if let Some(receiver) = state.cmux_open.take() {
        eprintln!("rev · waiting for pending cmux Markdown pane before cleanup");
        if let Ok(Ok(Some(surface))) = receiver.recv_timeout(Duration::from_millis(500)) {
            surface.close_detached().ok();
        }
    }
    if let Some(surface) = state.cmux_markdown.take() {
        surface.close_detached().ok();
    }
    state.markdown_server = None;
    result
}

fn recover_pending_questions(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
) -> Result<()> {
    let pending = storage.pending_ask_messages(&state.workspace.item.id)?;
    for message in pending {
        let Some((annotation, placement)) = state.annotations.iter().find(|(annotation, _)| {
            annotation.id == message.annotation_id && annotation.status == AnnotationStatus::Active
        }) else {
            continue;
        };
        let Some(repo) = state
            .workspace
            .repos
            .iter()
            .find(|repo| repo.record.id == annotation.repo_id)
        else {
            continue;
        };
        let Some(file) = repo
            .diff
            .files
            .iter()
            .find(|file| file.path() == annotation.file_path)
        else {
            continue;
        };
        let existing = storage.rev_question_session(&annotation.id)?;
        let prompt = if message.seq == 0 {
            question_prompt(
                &repo.record.name,
                file,
                placement,
                annotation,
                &message.text,
            )
        } else {
            message.text.clone()
        };
        queue_question_launch(
            state,
            paths,
            QuestionLaunch {
                annotation_id: annotation.id.clone(),
                pending: Some(PendingSend {
                    annotation_id: annotation.id.clone(),
                    user_message_id: message.id,
                    assistant_message_id: Uuid::new_v4().to_string(),
                    assistant_seq: message.seq + 1,
                    prompt,
                    needs_model_picker: existing.is_none() && state.default_model.is_none(),
                }),
                existing,
            },
        );
    }
    if !state.queued_questions.is_empty() {
        state.status = format!(
            "Resuming saved question work · {} waiting behind the active question",
            state.queued_questions.len()
        );
    }
    Ok(())
}

fn restore_terminal() {
    disable_raw_mode().ok();
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )
    .ok();
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    highlighter: &mut dyn Highlighter,
) -> Result<()> {
    loop {
        drain_refresh(state, storage)?;
        drain_agent_events(state, storage, paths)?;
        drain_model_catalog(state)?;
        drain_cmux_preview(state);
        if let Err(error) = sync_markdown_preview(state, paths) {
            state.status = format!("Markdown rich diff sync failed: {error:#}");
        }
        terminal.draw(|frame| render(frame, state, highlighter))?;
        if state.status == "quit" {
            return Ok(());
        }
        if !event::poll(POLL_INTERVAL)? {
            continue;
        }
        for _ in 0..MAX_EVENTS_PER_TICK {
            let event = event::read()?;
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Err(error) = handle_key(state, storage, paths, highlighter, key) {
                        state.status = format!("Action failed: {error}");
                        state.agent_activity = "UI action failed; review remains open".into();
                    }
                }
                Event::Paste(text) if state.mode == RevMode::Compose => {
                    state.compose.insert_str(state.compose_cursor, &text);
                    state.compose_cursor += text.len();
                    state.compose_scroll = u16::MAX;
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown => match state.mode {
                        RevMode::Normal | RevMode::Visual => move_row(state, highlighter, 3),
                        RevMode::Compose => {
                            state.compose_scroll = state.compose_scroll.saturating_add(3)
                        }
                        RevMode::History => {
                            state.history_cursor = (state.history_cursor + 3)
                                .min(state.annotations.len().saturating_sub(1))
                        }
                        RevMode::Help => state.help_scroll = state.help_scroll.saturating_add(3),
                        RevMode::FilePicker => move_file_picker(state, 3),
                        RevMode::Questions => move_question_cursor(state, 3),
                        RevMode::Command => {
                            let count = command_candidates(state).len();
                            state.command.selected =
                                (state.command.selected + 1).min(count.saturating_sub(1));
                        }
                        RevMode::Model => move_model_picker(state, 3),
                        RevMode::ConfirmClear | RevMode::ConfirmResolve => {}
                    },
                    MouseEventKind::ScrollUp => match state.mode {
                        RevMode::Normal | RevMode::Visual => move_row(state, highlighter, -3),
                        RevMode::Compose => {
                            state.compose_scroll = state.compose_scroll.saturating_sub(3)
                        }
                        RevMode::History => {
                            state.history_cursor = state.history_cursor.saturating_sub(3)
                        }
                        RevMode::Help => state.help_scroll = state.help_scroll.saturating_sub(3),
                        RevMode::FilePicker => move_file_picker(state, -3),
                        RevMode::Questions => move_question_cursor(state, -3),
                        RevMode::Command => {
                            state.command.selected = state.command.selected.saturating_sub(1)
                        }
                        RevMode::Model => move_model_picker(state, -3),
                        RevMode::ConfirmClear | RevMode::ConfirmResolve => {}
                    },
                    _ => {}
                },
                Event::Resize(_, _) => state.render_cache = None,
                _ => {}
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

fn handle_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    highlighter: &mut dyn Highlighter,
    key: KeyEvent,
) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('c')
        && state.agent.is_some()
    {
        if let Some(agent) = state.agent.as_ref() {
            agent.runtime.send(AgentCommand::Abort)?;
        }
        state.agent_activity = "Cancellation requested · waiting for Copilot to confirm…".into();
        state.agent_last_event = Instant::now();
        state.status =
            "Cancelling the active question · the UI remains responsive until it settles".into();
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('u'))
    {
        let direction = if key.code == KeyCode::Char('d') {
            1
        } else {
            -1
        };
        page_move(state, highlighter, direction);
        return Ok(());
    }
    if key.code == KeyCode::Char('q')
        && matches!(
            state.mode,
            RevMode::Normal
                | RevMode::Visual
                | RevMode::History
                | RevMode::FilePicker
                | RevMode::Questions
                | RevMode::Help
        )
    {
        state.pending_yank = false;
        state.history_delete_armed = None;
        if state.questions_open {
            close_questions(state);
        } else {
            open_questions(state);
        }
        return Ok(());
    }
    if key.code == KeyCode::Char('t')
        && matches!(
            state.mode,
            RevMode::Normal | RevMode::Visual | RevMode::FilePicker | RevMode::Questions
        )
    {
        state.pending_yank = false;
        state.history_delete_armed = None;
        if state.file_tree_open {
            close_file_picker(state);
        } else {
            open_file_picker(state);
        }
        return Ok(());
    }
    match state.mode {
        RevMode::Compose => handle_compose_key(state, storage, paths, key),
        RevMode::Command => handle_command_key(state, storage, paths, key),
        RevMode::Model => handle_model_key(state, storage, paths, key),
        RevMode::History => handle_history_key(state, storage, key),
        RevMode::FilePicker => {
            handle_file_picker_key(state, key);
            Ok(())
        }
        RevMode::Questions => {
            handle_question_key(state, paths, highlighter, key);
            Ok(())
        }
        RevMode::Help => {
            handle_help_key(state, key);
            Ok(())
        }
        RevMode::ConfirmClear => {
            match key.code {
                KeyCode::Char('y') => {
                    if state.agent.is_some() || !state.queued_questions.is_empty() {
                        state.mode = RevMode::Normal;
                        state.status =
                            "Cancel or finish active and queued questions before clearing".into();
                        return Ok(());
                    }
                    state.agent.take();
                    storage.clear_review_history(&state.workspace.item.id)?;
                    state.annotations.clear();
                    state.threads.clear();
                    state.question_sessions.clear();
                    state.invalidate_rows();
                    state.mode = RevMode::Normal;
                    state.status = "Workspace review history cleared".into();
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    state.mode = RevMode::Normal;
                    state.status = "Clear cancelled".into();
                }
                _ => {}
            }
            Ok(())
        }
        RevMode::ConfirmResolve => {
            match key.code {
                KeyCode::Char('y') => resolve_pending_annotation(state, storage)?,
                KeyCode::Char('n') | KeyCode::Esc => {
                    state.pending_resolve = None;
                    state.mode = RevMode::Normal;
                    state.status = "Resolve cancelled".into();
                }
                _ => {}
            }
            Ok(())
        }
        RevMode::Normal | RevMode::Visual => {
            handle_review_key(state, storage, paths, highlighter, key)
        }
    }
}

fn page_move(state: &mut RevState, highlighter: &mut dyn Highlighter, direction: isize) {
    let review_step = match state.mode {
        RevMode::Normal | RevMode::Visual | RevMode::Compose => {
            state.review_viewport_height.saturating_div(2).max(1)
        }
        _ => state
            .terminal_height
            .saturating_sub(5)
            .saturating_div(2)
            .max(1),
    };
    let signed = direction.saturating_mul(review_step as isize);
    match state.mode {
        RevMode::Normal | RevMode::Visual => {
            let old_cursor = state.row_cursor;
            let old_scroll = state.review_scroll;
            move_row(state, highlighter, signed);
            let moved = state.row_cursor.abs_diff(old_cursor);
            if direction > 0 {
                let count = row_count(state, highlighter);
                let max_scroll = count.saturating_sub(state.review_viewport_height);
                state.review_scroll = old_scroll.saturating_add(moved).min(max_scroll);
            } else {
                state.review_scroll = old_scroll.saturating_sub(moved);
            }
            state.status =
                "Moved cursor and viewport by half a page · Ctrl-D/Ctrl-U move down/up".into();
        }
        RevMode::Compose => {
            let step = review_step.min(u16::MAX as usize) as u16;
            if direction > 0 {
                state.compose_scroll = state.compose_scroll.saturating_add(step);
            } else {
                state.compose_scroll = state.compose_scroll.saturating_sub(step);
            }
            state.status = "Composer scrolled by half a page · Ctrl-D/Ctrl-U move down/up".into();
        }
        RevMode::History => {
            if direction > 0 {
                state.history_cursor = (state.history_cursor + review_step)
                    .min(state.annotations.len().saturating_sub(1));
            } else {
                state.history_cursor = state.history_cursor.saturating_sub(review_step);
            }
        }
        RevMode::FilePicker => move_file_picker(state, signed),
        RevMode::Questions => move_question_cursor(state, signed),
        RevMode::Help => {
            if direction > 0 {
                state.help_scroll = state
                    .help_scroll
                    .saturating_add(review_step.min(u16::MAX as usize) as u16);
            } else {
                state.help_scroll = state
                    .help_scroll
                    .saturating_sub(review_step.min(u16::MAX as usize) as u16);
            }
        }
        RevMode::Command => {
            let count = command_candidates(state).len();
            if direction > 0 {
                state.command.selected =
                    (state.command.selected + review_step).min(count.saturating_sub(1));
            } else {
                state.command.selected = state.command.selected.saturating_sub(review_step);
            }
        }
        RevMode::Model => move_model_picker(state, signed),
        RevMode::ConfirmClear | RevMode::ConfirmResolve => {}
    }
}

fn handle_review_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    highlighter: &mut dyn Highlighter,
    key: KeyEvent,
) -> Result<()> {
    if key.code != KeyCode::Char('y') {
        state.pending_yank = false;
    }
    if key.code != KeyCode::Char('d') {
        state.history_delete_armed = None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.visual_anchor = None;
        state.visual_row_anchor = None;
        state.mode = RevMode::Normal;
        state.status = "Selection cleared".into();
        return Ok(());
    }
    if state.mode == RevMode::Visual
        && state.visual_row_anchor.is_some()
        && matches!(
            key.code,
            KeyCode::Char('a')
                | KeyCode::Char('c')
                | KeyCode::Char('C')
                | KeyCode::Char('d')
                | KeyCode::Char('i')
                | KeyCode::Enter
        )
    {
        state.status =
            "VISUAL TEXT is copy-only · y copies · v/Esc clears before code actions".into();
        return Ok(());
    }
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            expand_hunk_edge(state, true)?;
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            expand_hunk_edge(state, false)?;
        }
        KeyCode::Char('o') if cursor_is_fold(state) => expand_fold(state, false)?,
        KeyCode::Char('O') if cursor_is_fold(state) => expand_fold(state, true)?,
        KeyCode::Char('j') | KeyCode::Down => move_row(state, highlighter, 1),
        KeyCode::Char('k') | KeyCode::Up => move_row(state, highlighter, -1),
        KeyCode::Char('h') | KeyCode::Left => state.move_file(false),
        KeyCode::Char('l') | KeyCode::Right => state.move_file(true),
        KeyCode::Char('M') => toggle_markdown_preview(state, paths)?,
        KeyCode::Char('g') | KeyCode::Home => state.row_cursor = 0,
        KeyCode::Char('G') | KeyCode::End => {
            let count = row_count(state, highlighter);
            state.row_cursor = count.saturating_sub(1);
        }
        KeyCode::Char('v') => {
            if state.mode == RevMode::Visual {
                state.visual_anchor = None;
                state.visual_row_anchor = None;
                state.mode = RevMode::Normal;
                state.status = "Selection cleared".into();
            } else {
                let cursor = state.row_cursor;
                let width = cached_row_width(state);
                let annotation_row = ensure_rows(state, width, highlighter)
                    .get(cursor)
                    .is_some_and(|row| matches!(row.kind, RevRowKind::Annotation { .. }));
                if annotation_row {
                    state.visual_row_anchor = Some(cursor);
                    state.visual_anchor = None;
                    state.mode = RevMode::Visual;
                    state.status =
                        "VISUAL TEXT · j/k extends through chat and code · y copies · Esc clears"
                            .into();
                } else if let Some(source) = current_source_index(state, highlighter) {
                    state.visual_anchor = Some(source);
                    state.visual_row_anchor = None;
                    state.mode = RevMode::Visual;
                    state.status =
                        "VISUAL LINE · j/k extends · y copies · a asks · c saves feedback".into();
                }
            }
        }
        KeyCode::Char('t') => {
            if state.file_tree_open {
                close_file_picker(state);
            } else {
                open_file_picker(state);
            }
        }
        KeyCode::Char('q') => {
            if state.questions_open {
                close_questions(state);
            } else {
                open_questions(state);
            }
        }
        KeyCode::Char('Q') => {
            if state.questions_open {
                close_questions(state);
            } else {
                open_questions(state);
            }
        }
        KeyCode::Char('y') => {
            if state.mode == RevMode::Visual {
                yank_visual_selection(state, highlighter)?;
            } else if state.pending_yank {
                yank_current_item(state, highlighter)?;
            } else {
                state.pending_yank = true;
                state.status =
                    "YANK · press y again to copy the current line or chat message".into();
            }
        }
        KeyCode::Char('d') => delete_contextual_comment(state, storage, highlighter)?,
        KeyCode::Char('m') => {
            if let Some(id) = current_question_id_from_cache(state) {
                start_model_switch(state, paths, &id);
            } else {
                state.status =
                    "Move onto a question thread or press Q to choose one before switching models"
                        .into();
            }
        }
        KeyCode::Char('a') => {
            match contextual_annotation(state, highlighter, AnnotationKind::Ask) {
                ContextualAnnotationMatch::Match(id) => begin_follow_up(state, highlighter, id),
                ContextualAnnotationMatch::None => {
                    begin_compose(state, highlighter, ComposeTarget::NewQuestion)
                }
                ContextualAnnotationMatch::Blocked(message) => state.status = message,
            }
        }
        KeyCode::Char('c') => {
            match contextual_annotation(state, highlighter, AnnotationKind::Comment) {
                ContextualAnnotationMatch::Match(id) => begin_edit_feedback(state, highlighter, id),
                ContextualAnnotationMatch::None => {
                    begin_compose(state, highlighter, ComposeTarget::Feedback)
                }
                ContextualAnnotationMatch::Blocked(message) => state.status = message,
            }
        }
        KeyCode::Char('i') | KeyCode::Enter => {
            if let Some(id) = current_annotation_id(state, highlighter) {
                if state
                    .annotation(&id)
                    .is_some_and(|(annotation, _)| annotation.kind == AnnotationKind::Ask)
                {
                    begin_follow_up(state, highlighter, id);
                }
            }
        }
        KeyCode::Char('r') => {
            if state.mode == RevMode::Visual && state.visual_anchor.is_some() {
                retract_selected_context(state, highlighter)?;
            } else if let Some(id) = current_annotation_id(state, highlighter) {
                begin_resolve(state, id);
            } else {
                state.mode = RevMode::History;
                state.history_cursor = 0;
                state.status =
                    "HISTORY · j/k move · u reopens · d d deletes feedback · Esc return".into();
            }
        }
        KeyCode::Char('e') => copy_feedback_prompt(state, storage)?,
        KeyCode::Char('C') => {
            match contextual_annotation(state, highlighter, AnnotationKind::Comment) {
                ContextualAnnotationMatch::Match(id) => begin_edit_feedback(state, highlighter, id),
                ContextualAnnotationMatch::None => {
                    state.status =
                        "Select exactly the original commented lines before editing feedback".into()
                }
                ContextualAnnotationMatch::Blocked(message) => state.status = message,
            }
        }
        KeyCode::Char(':') => {
            state.mode = RevMode::Command;
            state.command = CommandPalette::default();
            state.status = "COMMAND · type to filter · ↑/↓ select · Enter run · Esc return".into();
        }
        KeyCode::Esc => {
            state.mode = RevMode::Normal;
            state.visual_anchor = None;
            state.visual_row_anchor = None;
            state.status = "NORMAL".into();
        }
        _ => {}
    }
    let _ = paths;
    Ok(())
}

fn open_file_picker(state: &mut RevState) {
    state.file_picker_return_mode = if state.mode == RevMode::Visual {
        RevMode::Visual
    } else {
        RevMode::Normal
    };
    if let Some((repo_index, _)) = state.current_indices() {
        state.collapsed_repos.remove(&repo_index);
    }
    state.file_tree_open = true;
    state.mode = RevMode::FilePicker;
    state.file_picker_cursor = file_picker_rows(state)
        .iter()
        .position(|row| {
            matches!(
                row,
                FilePickerRow::File { flat_index, .. } if *flat_index == state.file_index
            )
        })
        .unwrap_or(0);
    state.status = "FILES · j/k previews immediately · h/l fold · Enter/t returns to editor".into();
}

fn close_file_picker(state: &mut RevState) {
    state.file_tree_open = false;
    if state.questions_open
        && state.file_picker_return_mode == RevMode::Visual
        && (state.visual_anchor.is_some() || state.visual_row_anchor.is_some())
    {
        state.question_return_mode = RevMode::Visual;
    }
    state.mode = if state.questions_open {
        RevMode::Questions
    } else if state.file_picker_return_mode == RevMode::Visual
        && (state.visual_anchor.is_some() || state.visual_row_anchor.is_some())
    {
        RevMode::Visual
    } else {
        RevMode::Normal
    };
    state.status = if state.mode == RevMode::Visual {
        "Selection retained · j/k extends · v/Esc clears".into()
    } else {
        "Back to review".into()
    };
}

fn handle_file_picker_key(state: &mut RevState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('t') | KeyCode::Esc => close_file_picker(state),
        KeyCode::Char('j') | KeyCode::Down => move_file_picker(state, 1),
        KeyCode::Char('k') | KeyCode::Up => move_file_picker(state, -1),
        KeyCode::Char('g') | KeyCode::Home => {
            state.file_picker_cursor = 0;
            preview_file_picker_selection(state);
        }
        KeyCode::Char('G') | KeyCode::End => {
            state.file_picker_cursor = file_picker_rows(state).len().saturating_sub(1);
            preview_file_picker_selection(state);
        }
        KeyCode::Char('h') | KeyCode::Left => collapse_picker_repo(state),
        KeyCode::Char('l') | KeyCode::Right => expand_picker_repo(state),
        KeyCode::Enter => activate_file_picker_row(state),
        _ => {}
    }
}

fn move_file_picker(state: &mut RevState, delta: isize) {
    let row_count = file_picker_rows(state).len();
    if delta >= 0 {
        state.file_picker_cursor =
            (state.file_picker_cursor + delta as usize).min(row_count.saturating_sub(1));
    } else {
        state.file_picker_cursor = state
            .file_picker_cursor
            .saturating_sub(delta.unsigned_abs());
    }
    preview_file_picker_selection(state);
}

fn preview_file_picker_selection(state: &mut RevState) {
    let Some(FilePickerRow::File { flat_index, .. }) = selected_file_picker_row(state) else {
        return;
    };
    state.switch_file(flat_index);
    state.status =
        "FILES · preview updated at this file's last position · Enter/t returns to editor".into();
}

fn file_picker_rows(state: &RevState) -> Vec<FilePickerRow> {
    let mut rows = Vec::new();
    for repo_index in 0..state.workspace.repos.len() {
        rows.push(FilePickerRow::Repo(repo_index));
        if state.collapsed_repos.contains(&repo_index) {
            continue;
        }
        rows.extend(
            state
                .files
                .iter()
                .enumerate()
                .filter(|(_, (candidate_repo, _))| *candidate_repo == repo_index)
                .map(|(flat_index, _)| FilePickerRow::File {
                    repo_index,
                    flat_index,
                }),
        );
    }
    rows
}

fn selected_file_picker_row(state: &RevState) -> Option<FilePickerRow> {
    let rows = file_picker_rows(state);
    rows.get(state.file_picker_cursor.min(rows.len().saturating_sub(1)))
        .copied()
}

fn sync_file_picker_cursor(state: &mut RevState) {
    let Some((repo_index, _)) = state.current_indices() else {
        state.file_picker_cursor = 0;
        return;
    };
    if state.file_tree_open {
        state.collapsed_repos.remove(&repo_index);
    }
    state.file_picker_cursor = file_picker_rows(state)
        .iter()
        .position(|row| {
            matches!(
                row,
                FilePickerRow::File { flat_index, .. } if *flat_index == state.file_index
            )
        })
        .unwrap_or(0);
}

fn selected_file_picker_repo(state: &RevState) -> Option<usize> {
    match selected_file_picker_row(state)? {
        FilePickerRow::Repo(repo_index) | FilePickerRow::File { repo_index, .. } => {
            Some(repo_index)
        }
    }
}

fn collapse_picker_repo(state: &mut RevState) {
    let Some(repo_index) = selected_file_picker_repo(state) else {
        return;
    };
    state.collapsed_repos.insert(repo_index);
    state.file_picker_cursor = file_picker_rows(state)
        .iter()
        .position(|row| matches!(row, FilePickerRow::Repo(index) if *index == repo_index))
        .unwrap_or(0);
    state.status = "Repository collapsed · l/Right expands".into();
}

fn expand_picker_repo(state: &mut RevState) {
    let Some(repo_index) = selected_file_picker_repo(state) else {
        return;
    };
    state.collapsed_repos.remove(&repo_index);
    state.status = "Repository expanded · h/Left collapses".into();
}

fn activate_file_picker_row(state: &mut RevState) {
    if let Some(FilePickerRow::File { flat_index, .. }) = selected_file_picker_row(state) {
        state.switch_file(flat_index);
    }
    close_file_picker(state);
    state.status = "Back to the previewed file at its previous position".into();
}

fn question_ids(state: &RevState) -> Vec<String> {
    state
        .annotations
        .iter()
        .filter(|(annotation, _)| {
            annotation.kind == AnnotationKind::Ask && annotation.status == AnnotationStatus::Active
        })
        .map(|(annotation, _)| annotation.id.clone())
        .collect()
}

fn selected_question_id(state: &RevState) -> Option<String> {
    let ids = question_ids(state);
    ids.get(state.question_cursor.min(ids.len().saturating_sub(1)))
        .cloned()
}

fn current_question_id_from_cache(state: &RevState) -> Option<String> {
    let annotation_id = match &state
        .render_cache
        .as_ref()?
        .rows
        .get(state.row_cursor)?
        .kind
    {
        RevRowKind::Annotation { annotation_id, .. } => annotation_id,
        RevRowKind::Source { .. } => return None,
    };
    state
        .annotation(annotation_id)
        .filter(|(annotation, _)| annotation.kind == AnnotationKind::Ask)
        .map(|_| annotation_id.clone())
}

fn open_questions(state: &mut RevState) {
    let ids = question_ids(state);
    state.question_return_mode = if state.mode == RevMode::FilePicker && state.file_tree_open {
        RevMode::FilePicker
    } else if state.mode == RevMode::Visual {
        RevMode::Visual
    } else if matches!(state.mode, RevMode::History | RevMode::Help) {
        state.mode
    } else {
        RevMode::Normal
    };
    if let Some(current) = current_question_id_from_cache(state) {
        state.question_cursor = ids.iter().position(|id| id == &current).unwrap_or(0);
    } else {
        state.question_cursor = state.question_cursor.min(ids.len().saturating_sub(1));
    }
    state.questions_open = true;
    state.mode = RevMode::Questions;
    state.status = if ids.is_empty() {
        "QUESTIONS · no threads yet · q/Esc closes · select code and press a to ask".into()
    } else {
        "QUESTIONS · j/k choose · Enter jump · a continue · m model · q/Esc close".into()
    };
}

fn close_questions(state: &mut RevState) {
    state.questions_open = false;
    state.mode = if state.question_return_mode == RevMode::FilePicker && state.file_tree_open {
        RevMode::FilePicker
    } else if state.question_return_mode == RevMode::Visual
        && (state.visual_anchor.is_some() || state.visual_row_anchor.is_some())
    {
        RevMode::Visual
    } else if matches!(state.question_return_mode, RevMode::History | RevMode::Help) {
        state.question_return_mode
    } else {
        RevMode::Normal
    };
    state.status = if state.mode == RevMode::Visual {
        "Selection retained · j/k extends · v/Esc clears".into()
    } else {
        "Back to review".into()
    };
}

fn move_question_cursor(state: &mut RevState, delta: isize) {
    let count = question_ids(state).len();
    if delta >= 0 {
        state.question_cursor =
            (state.question_cursor + delta as usize).min(count.saturating_sub(1));
    } else {
        state.question_cursor = state.question_cursor.saturating_sub(delta.unsigned_abs());
    }
}

fn handle_question_key(
    state: &mut RevState,
    paths: &AppPaths,
    highlighter: &mut dyn Highlighter,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Char('Q') | KeyCode::Esc => close_questions(state),
        KeyCode::Char('j') | KeyCode::Down => move_question_cursor(state, 1),
        KeyCode::Char('k') | KeyCode::Up => move_question_cursor(state, -1),
        KeyCode::Char('g') | KeyCode::Home => state.question_cursor = 0,
        KeyCode::Char('G') | KeyCode::End => {
            state.question_cursor = question_ids(state).len().saturating_sub(1)
        }
        KeyCode::Enter => {
            if let Some(id) = selected_question_id(state) {
                focus_question(state, highlighter, &id);
            }
        }
        KeyCode::Char('i') | KeyCode::Char('a') => {
            if let Some(id) = selected_question_id(state) {
                if focus_question(state, highlighter, &id) {
                    begin_follow_up(state, highlighter, id);
                }
            }
        }
        KeyCode::Char('m') => {
            if let Some(id) = selected_question_id(state) {
                start_model_switch(state, paths, &id);
            }
        }
        _ => {}
    }
}

fn focus_question(
    state: &mut RevState,
    highlighter: &mut dyn Highlighter,
    annotation_id: &str,
) -> bool {
    let Some((annotation, _)) = state.annotation(annotation_id).cloned() else {
        state.status = "Question no longer exists".into();
        return false;
    };
    let file_index = state.files.iter().position(|(repo_index, file_index)| {
        let repo = &state.workspace.repos[*repo_index];
        repo.record.id == annotation.repo_id
            && repo.diff.files[*file_index].path() == annotation.file_path
    });
    let Some(file_index) = file_index else {
        state.status = "This historical question's file is no longer in the current diff".into();
        return false;
    };
    state.switch_file(file_index);
    state.row_cursor = 0;
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    sync_file_picker_cursor(state);
    state.mode = RevMode::Normal;
    state.render_cache = None;
    let width = cached_row_width(state);
    let rows = ensure_rows(state, width, highlighter);
    state.row_cursor = rows
        .iter()
        .position(|row| {
            matches!(
                &row.kind,
                RevRowKind::Annotation {
                    annotation_id: candidate,
                    ..
                } if candidate == annotation_id
            )
        })
        .unwrap_or(0);
    state.status = "Question focused · i/Enter continues this thread · m switches model".into();
    true
}

fn begin_follow_up(state: &mut RevState, highlighter: &mut dyn Highlighter, annotation_id: String) {
    let session_starting = state
        .agent
        .as_ref()
        .is_some_and(|agent| agent.question_id == annotation_id)
        || state
            .queued_questions
            .iter()
            .any(|launch| launch.annotation_id == annotation_id);
    if !state.question_sessions.contains_key(&annotation_id) && session_starting {
        state.status =
            "This question session is still starting · wait for it to become ready before following up"
                .into();
        return;
    }
    begin_compose(state, highlighter, ComposeTarget::FollowUp(annotation_id));
    state.status =
        "FOLLOW-UP · Enter sends · /clear resets this thread · Shift-Enter newline · Esc cancels"
            .into();
}

fn contextual_annotation(
    state: &mut RevState,
    highlighter: &mut dyn Highlighter,
    kind: AnnotationKind,
) -> ContextualAnnotationMatch {
    if state.mode != RevMode::Visual {
        if let Some(annotation_id) = current_annotation_id(state, highlighter) {
            let Some((annotation, _)) = state.annotation(&annotation_id) else {
                return ContextualAnnotationMatch::None;
            };
            if annotation.kind == kind {
                return ContextualAnnotationMatch::Match(annotation_id);
            }
            return ContextualAnnotationMatch::Blocked(format!(
                "This row is {} · move onto source lines or the intended {}",
                annotation.kind.as_str(),
                if kind == AnnotationKind::Ask {
                    "question"
                } else {
                    "feedback"
                }
            ));
        }
    }
    let Some((start, end)) = selected_source_range(state, highlighter) else {
        return ContextualAnnotationMatch::None;
    };
    let Some((repo_index, file_index)) = state.current_indices() else {
        return ContextualAnnotationMatch::None;
    };
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    let Ok(anchor) = anchor_from_diff(file, start, end) else {
        return ContextualAnnotationMatch::None;
    };
    let matches = state
        .annotations
        .iter()
        .rev()
        .filter(|(annotation, placement)| {
            annotation.kind == kind
                && annotation.status == AnnotationStatus::Active
                && annotation.repo_id == repo.record.id
                && annotation.file_path == file.display_path
                && !placement.outdated
                && !placement.ambiguous
                && placement.side == anchor.side
                && placement.line_start == anchor.line_start as i64
                && placement.line_end == anchor.line_end as i64
        })
        .map(|(annotation, _)| annotation.id.clone())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => ContextualAnnotationMatch::None,
        [annotation_id] => ContextualAnnotationMatch::Match(annotation_id.clone()),
        _ => ContextualAnnotationMatch::Blocked(format!(
            "Multiple saved {} items share this exact selection · move onto the intended inline item",
            kind.as_str()
        )),
    }
}

fn begin_edit_feedback(
    state: &mut RevState,
    highlighter: &mut dyn Highlighter,
    annotation_id: String,
) {
    let Some((annotation, placement)) = state.annotation(&annotation_id).cloned() else {
        state.status = "That feedback no longer exists".into();
        return;
    };
    if annotation.kind != AnnotationKind::Comment {
        state.status = "Only saved feedback can be edited with c/C".into();
        return;
    }
    if placement.outdated || placement.ambiguous {
        state.status =
            "This feedback anchor is outdated or ambiguous · reselect exact current lines to create new feedback"
                .into();
        return;
    }
    let Some((start, end)) = visible_selection_for_placement(state, &annotation, &placement) else {
        state.status =
            "The original commented range is not visible · reveal its context before editing"
                .into();
        return;
    };
    state.visual_anchor = Some(start);
    state.visual_row_anchor = None;
    state.mode = RevMode::Compose;
    state.compose_target = Some(ComposeTarget::EditFeedback(annotation_id));
    state.compose = annotation.text.unwrap_or_default();
    state.compose_cursor = state.compose.len();
    state.compose_scroll = u16::MAX;
    let previous_cursor = state.row_cursor;
    let width = cached_row_width(state);
    let rows = ensure_rows(state, width, highlighter);
    state.row_cursor = rows
        .iter()
        .position(|row| {
            matches!(
                row.kind,
                RevRowKind::Source {
                    visible_index,
                    ..
                } if visible_index == end
            )
        })
        .unwrap_or(previous_cursor);
    state.status =
        "EDIT FEEDBACK · original selection restored · Enter saves · Esc keeps selection".into();
}

fn visible_selection_for_placement(
    state: &RevState,
    annotation: &Annotation,
    placement: &Placement,
) -> Option<(usize, usize)> {
    let (repo_index, file_index) = state.current_indices()?;
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    if annotation.repo_id != repo.record.id || annotation.file_path != file.display_path {
        return None;
    }
    let count = usize::try_from(annotation.anchor_line_count).ok()?.max(1);
    let line_count = file.visible_lines().count();
    (0..line_count).find_map(|start| {
        let end = start.checked_add(count - 1)?;
        if end >= line_count {
            return None;
        }
        let anchor = anchor_from_diff(file, start, end).ok()?;
        (anchor.side == placement.side
            && anchor.line_start as i64 == placement.line_start
            && anchor.line_end as i64 == placement.line_end)
            .then_some((start, end))
    })
}

fn start_model_switch(state: &mut RevState, paths: &AppPaths, annotation_id: &str) {
    if state.agent.is_some() || !state.queued_questions.is_empty() {
        state.status =
            "Finish or cancel active and queued questions before switching a thread's model".into();
        return;
    }
    let Some(session) = state.question_sessions.get(annotation_id).cloned() else {
        state.status =
            "This question has no ready Copilot session yet, so its model cannot be changed".into();
        return;
    };
    state.model_return_mode = if state.mode == RevMode::Questions {
        RevMode::Questions
    } else {
        RevMode::Normal
    };
    start_question_agent(
        state,
        paths,
        QuestionLaunch {
            annotation_id: annotation_id.to_owned(),
            pending: None,
            existing: Some(session),
        },
    );
}

fn start_global_model_picker(state: &mut RevState) {
    if state.agent.is_some() || !state.queued_questions.is_empty() {
        state.mode = RevMode::Normal;
        state.status =
            "Finish or cancel active and queued questions before changing the global default"
                .into();
        return;
    }
    if state.model_catalog.is_some() {
        state.status = "Global model choices are already loading · UI remains responsive".into();
        return;
    }
    state.model_catalog = Some(start_model_catalog(state.workspace.session_root.clone()));
    state.model_catalog_started = Some(Instant::now());
    state.mode = RevMode::Normal;
    state.agent_activity =
        "Loading Copilot model catalog without creating a question session…".into();
    state.agent_last_event = Instant::now();
    state.status =
        "Loading global model choices in the background · navigation remains active".into();
}

fn drain_model_catalog(state: &mut RevState) -> Result<()> {
    let Some(receiver) = state.model_catalog.as_ref() else {
        return Ok(());
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => {
            if state
                .model_catalog_started
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(2))
            {
                let elapsed = state
                    .model_catalog_started
                    .map_or(0, |started| started.elapsed().as_secs());
                state.status = format!(
                    "Loading global Copilot models · {elapsed}s elapsed · navigation remains active"
                );
                state.agent_activity =
                    format!("Copilot model catalog is still loading · {elapsed}s elapsed");
            }
            return Ok(());
        }
        Err(TryRecvError::Disconnected) => {
            state.model_catalog = None;
            state.model_catalog_started = None;
            state.status = "Copilot model catalog stopped unexpectedly".into();
            return Ok(());
        }
    };
    state.model_catalog = None;
    state.model_catalog_started = None;
    let models = match result {
        Ok(models) => models,
        Err(error) => {
            state.status = format!("Could not load Copilot models: {error:#}");
            state.agent_activity =
                "Model catalog failed · existing defaults and question sessions are unchanged"
                    .into();
            return Ok(());
        }
    };
    if models.is_empty() {
        state.status =
            "Copilot returned no selectable models · existing defaults are unchanged".into();
        state.agent_activity = "Model catalog completed with no selectable models".into();
        return Ok(());
    }
    if state.agent.is_some() || !state.queued_questions.is_empty() {
        state.status =
            "Model catalog loaded, but question work started · run :model again when it finishes"
                .into();
        state.agent_activity =
            "Global model change deferred so active question sessions remain isolated".into();
        return Ok(());
    }
    let initial_selection = state
        .default_model
        .clone()
        .unwrap_or_else(|| ModelSelection {
            model_id: models[0].id.clone(),
            reasoning_effort: None,
            context_tier: None,
        });
    state.picker = Some(ModelPicker::new(
        models,
        initial_selection,
        PickerScope::GlobalDefault,
    ));
    state.mode = RevMode::Model;
    state.status = "GLOBAL MODEL 1/3 · type to search · ↑/↓ choose · Enter deeper".into();
    Ok(())
}

fn begin_compose(state: &mut RevState, highlighter: &mut dyn Highlighter, target: ComposeTarget) {
    if matches!(target, ComposeTarget::Feedback | ComposeTarget::NewQuestion) {
        let Some((start, end)) = selected_source_range(state, highlighter) else {
            state.status = "Move to a source row before adding feedback or a question".into();
            return;
        };
        if state.current_file().is_some_and(|file| {
            file.visible_lines()
                .enumerate()
                .any(|(index, line)| (start..=end).contains(&index) && line.kind == LineKind::Meta)
        }) {
            state.status =
                "Fold rows cannot be annotated · move onto source or reveal context first".into();
            return;
        }
    }
    state.mode = RevMode::Compose;
    state.compose_target = Some(target);
    state.compose.clear();
    state.compose_cursor = 0;
    state.compose_scroll = 0;
    state.status = "INSERT · Enter submit · Shift-Enter newline · Esc cancel".into();
}

fn handle_command_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    key: KeyEvent,
) -> Result<()> {
    let candidates = command_candidates(state);
    match key.code {
        KeyCode::Esc => {
            state.mode = RevMode::Normal;
            state.status = "Command cancelled".into();
        }
        KeyCode::Up => {
            state.command.selected = state.command.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            state.command.selected =
                (state.command.selected + 1).min(candidates.len().saturating_sub(1));
        }
        KeyCode::Tab => {
            if let Some(candidate) = candidates.get(state.command.selected) {
                state.command.input.clone_from(candidate);
                state.command.cursor = state.command.input.len();
            }
        }
        KeyCode::Enter => {
            let typed = state.command.input.trim();
            let command = candidates
                .iter()
                .find(|candidate| candidate.as_str() == typed)
                .cloned()
                .or_else(|| candidates.get(state.command.selected).cloned())
                .unwrap_or_else(|| typed.to_owned());
            execute_command(state, storage, paths, &command)?;
        }
        KeyCode::Backspace => {
            if state.command.cursor > 0 {
                let previous =
                    previous_grapheme_boundary(&state.command.input, state.command.cursor);
                state
                    .command
                    .input
                    .replace_range(previous..state.command.cursor, "");
                state.command.cursor = previous;
                state.command.selected = 0;
            }
        }
        KeyCode::Delete => {
            let next = next_grapheme_boundary(&state.command.input, state.command.cursor);
            if next > state.command.cursor {
                state
                    .command
                    .input
                    .replace_range(state.command.cursor..next, "");
            }
        }
        KeyCode::Left => {
            state.command.cursor =
                previous_grapheme_boundary(&state.command.input, state.command.cursor);
        }
        KeyCode::Right => {
            state.command.cursor =
                next_grapheme_boundary(&state.command.input, state.command.cursor);
        }
        KeyCode::Home => state.command.cursor = 0,
        KeyCode::End => state.command.cursor = state.command.input.len(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.command.input.insert(state.command.cursor, character);
            state.command.cursor += character.len_utf8();
            state.command.selected = 0;
        }
        _ => {}
    }
    Ok(())
}

fn command_candidates(state: &RevState) -> Vec<String> {
    let input = state.command.input.trim().to_ascii_lowercase();
    let mut commands = vec![
        "diff unified".to_owned(),
        "diff split".to_owned(),
        "expand above".to_owned(),
        "expand below".to_owned(),
        "export feedback".to_owned(),
        "help".to_owned(),
        "hide comments".to_owned(),
        "hide questions".to_owned(),
        "history".to_owned(),
        "model".to_owned(),
        "model reset".to_owned(),
        "questions".to_owned(),
        "refresh".to_owned(),
        "retract all".to_owned(),
        "render markdown".to_owned(),
        "render markdown close".to_owned(),
        "clear".to_owned(),
        "show comments".to_owned(),
        "show questions".to_owned(),
        "q".to_owned(),
        "quit".to_owned(),
    ];
    commands.extend(
        state
            .branch_candidates
            .iter()
            .map(|branch| format!("base {branch}")),
    );
    commands.sort();
    commands
        .into_iter()
        .filter(|candidate| {
            input.is_empty()
                || candidate.to_ascii_lowercase().contains(&input)
                || (input.starts_with("base ")
                    && candidate.starts_with("base ")
                    && candidate
                        .to_ascii_lowercase()
                        .contains(input.trim_start_matches("base ").trim()))
        })
        .collect()
}

fn execute_command(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    command: &str,
) -> Result<()> {
    let command = command.trim();
    match command {
        "help" | "?" => {
            state.mode = RevMode::Help;
            state.help_scroll = 0;
            state.status = "j/k or arrows scroll · Esc closes · q opens questions".into();
        }
        "questions" | "threads" => open_questions(state),
        "model" => {
            start_global_model_picker(state);
        }
        "model reset" => {
            if state.agent.is_some()
                || !state.queued_questions.is_empty()
                || state.model_catalog.is_some()
            {
                state.mode = RevMode::Normal;
                state.status =
                    "Finish question work and model loading before resetting the global default"
                        .into();
                return Ok(());
            }
            storage.delete_setting(REV_DEFAULT_MODEL_SETTING)?;
            state.default_model = None;
            state.mode = RevMode::Normal;
            state.status =
                "Global question model reset · new threads will ask you to choose".into();
        }
        "diff unified" | "unified" => {
            state.diff_layout = RevDiffLayout::Unified;
            state.invalidate_rows();
            state.mode = RevMode::Normal;
            state.status = "Diff layout changed to unified".into();
        }
        "diff split" | "split" => {
            state.diff_layout = RevDiffLayout::Split;
            state.invalidate_rows();
            state.mode = RevMode::Normal;
            state.status = "Diff layout changed to split".into();
        }
        "expand above" => {
            state.mode = RevMode::Normal;
            expand_hunk_edge(state, true)?;
        }
        "expand below" => {
            state.mode = RevMode::Normal;
            expand_hunk_edge(state, false)?;
        }
        "hide comments" => {
            state.show_comments = false;
            state.invalidate_rows();
            state.mode = RevMode::Normal;
            state.status =
                "Comments hidden from the diff · they remain saved · :show comments restores them"
                    .into();
        }
        "show comments" => {
            state.show_comments = true;
            state.invalidate_rows();
            state.mode = RevMode::Normal;
            state.status = "Comments are visible in the diff".into();
        }
        "hide questions" => {
            state.show_questions = false;
            state.invalidate_rows();
            state.mode = RevMode::Normal;
            state.status =
                "Questions hidden from the diff · threads remain saved · :show questions restores them"
                    .into();
        }
        "show questions" => {
            state.show_questions = true;
            state.invalidate_rows();
            state.mode = RevMode::Normal;
            state.status = "Questions are visible in the diff".into();
        }
        "retract all" => {
            state.mode = RevMode::Normal;
            retract_all_context(state)?;
        }
        "export feedback" | "feedback" | "export" => {
            state.mode = RevMode::Normal;
            copy_feedback_prompt(state, storage)?;
        }
        "history" => {
            state.mode = RevMode::History;
            state.history_cursor = 0;
            state.status =
                "HISTORY · j/k move · d d deletes saved feedback only · Esc return".into();
        }
        "refresh" => {
            state.mode = RevMode::Normal;
            start_refresh(state, paths)?;
        }
        "render markdown" | "render-markdown" | "markdown" => {
            state.mode = RevMode::Normal;
            if !state.markdown_preview {
                toggle_markdown_preview(state, paths)?;
            } else {
                state.status =
                    "Markdown rich diff is already open · navigation is synchronized · M closes"
                        .into();
            }
        }
        "render markdown cmux" | "render-markdown cmux" => {
            state.mode = RevMode::Normal;
            if !state.markdown_preview {
                toggle_markdown_preview(state, paths)?;
            } else {
                state.status =
                    "Markdown rich diff is already open · navigation is synchronized · M closes"
                        .into();
            }
        }
        "render markdown close" | "render-markdown close" => {
            state.mode = RevMode::Normal;
            close_markdown_preview(state);
        }
        "clear" => {
            state.mode = RevMode::ConfirmClear;
            state.status = "Clear every saved comment and question here? y/n".into();
        }
        "quit" | "q" => {
            if state.agent.is_some() || !state.queued_questions.is_empty() {
                state.mode = RevMode::Normal;
                state.status =
                    "Questions are active or queued · Ctrl-C cancels the active one before :quit"
                        .into();
            } else {
                state.status = "quit".into();
            }
        }
        _ if command.starts_with("base ") => {
            if state
                .current_repo()
                .is_some_and(|repo| repo.record.remote_pr_url.is_some())
            {
                state.mode = RevMode::Normal;
                state.status =
                    "GitHub PR bases come from GitHub · use :refresh after the PR base changes"
                        .into();
                return Ok(());
            }
            if state.streaming || state.agent.is_some() || !state.queued_questions.is_empty() {
                state.mode = RevMode::Normal;
                state.status =
                    "Finish or cancel queued questions before changing the diff base".into();
                return Ok(());
            }
            let branch = command.trim_start_matches("base ").trim();
            if branch.is_empty() {
                state.status = "Choose a base branch from the autocomplete list".into();
                return Ok(());
            }
            let workspace_root = state.workspace.item.workspace_root.clone();
            let layout = state.diff_layout;
            let show_comments = state.show_comments;
            let show_questions = state.show_questions;
            let mut selected_repo = state
                .current_repo()
                .map(|repo| repo.record.clone())
                .context("no current repository")?;
            let previous_repo = selected_repo.clone();
            selected_repo.base_branch = Some(branch.to_owned());
            selected_repo.base_branch_source = BaseBranchSource::PerRepo;
            storage.upsert_repo(&selected_repo)?;
            let workspace = match resolve_local(&workspace_root, None, paths, storage) {
                Ok(workspace) => workspace,
                Err(error) => {
                    storage.upsert_repo(&previous_repo)?;
                    return Err(error);
                }
            };
            let mut replacement = RevState::load(workspace, storage)?;
            replacement.diff_layout = layout;
            replacement.show_comments = show_comments;
            replacement.show_questions = show_questions;
            replacement.status =
                format!("Diff base for {} changed to {branch}", selected_repo.name);
            *state = replacement;
        }
        "" => {
            state.mode = RevMode::Normal;
            state.status = "No command entered".into();
        }
        _ => {
            state.status = format!("Unknown command: {command}");
        }
    }
    Ok(())
}

fn start_refresh(state: &mut RevState, paths: &AppPaths) -> Result<()> {
    if state.refresh.is_some() {
        state.status = "Refresh already running · the review remains usable".into();
        return Ok(());
    }
    if state.agent.is_some() || !state.queued_questions.is_empty() {
        state.status = "Finish or cancel active questions before refreshing the revision".into();
        return Ok(());
    }
    let remote = state
        .workspace
        .repos
        .iter()
        .filter_map(|repo| repo.record.remote_pr_url.as_deref())
        .map(PrReference::parse)
        .collect::<Result<Vec<_>>>()?;
    let local_root = state.workspace.item.workspace_root.clone();
    let worker_paths = paths.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = Storage::open(&worker_paths.database).and_then(|storage| {
            if remote.is_empty() {
                resolve_local(&local_root, None, &worker_paths, &storage)
            } else {
                RemoteResolver::default().resolve(&remote, &worker_paths, &storage)
            }
        });
        sender.send(result).ok();
    });
    state.refresh = Some(receiver);
    state.refresh_started = Some(Instant::now());
    state.status =
        "REFRESHING in background · navigation remains active · :refresh shows status".into();
    Ok(())
}

fn drain_refresh(state: &mut RevState, storage: &Storage) -> Result<()> {
    let Some(receiver) = state.refresh.as_ref() else {
        return Ok(());
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => {
            if state
                .refresh_started
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(2))
            {
                let elapsed = state
                    .refresh_started
                    .map_or(0, |started| started.elapsed().as_secs());
                state.status = format!(
                    "REFRESHING in background · {elapsed}s elapsed · navigation remains active"
                );
            }
            return Ok(());
        }
        Err(TryRecvError::Disconnected) => {
            state.refresh = None;
            state.refresh_started = None;
            state.status = "Refresh worker stopped unexpectedly · review remains open".into();
            return Ok(());
        }
    };
    state.refresh = None;
    let elapsed = state
        .refresh_started
        .take()
        .map_or(0, |time| time.elapsed().as_millis());
    match result {
        Ok(workspace) => {
            for repo in &workspace.repos {
                storage.mark_version_opened(&repo.version.id)?;
            }
            let selected = state.current_file().map(|file| file.path().to_path_buf());
            let layout = state.diff_layout;
            let markdown_preview = state.markdown_preview;
            let cmux_markdown_requested = state.cmux_markdown_requested;
            let cmux_markdown = state.cmux_markdown.take();
            let cmux_open = state.cmux_open.take();
            let cmux_close = state.cmux_close.take();
            let markdown_server = state.markdown_server.take();
            let mut replacement = RevState::load(workspace, storage)?;
            replacement.diff_layout = layout;
            replacement.markdown_preview = markdown_preview;
            replacement.cmux_markdown_requested = cmux_markdown_requested;
            replacement.cmux_markdown = cmux_markdown;
            replacement.cmux_open = cmux_open;
            replacement.cmux_close = cmux_close;
            replacement.markdown_server = markdown_server;
            if let Some(selected) = selected {
                if let Some(index) = replacement.files.iter().position(|(repo, file)| {
                    replacement.workspace.repos[*repo].diff.files[*file].path() == selected
                }) {
                    replacement.file_index = index;
                }
            }
            replacement.status =
                format!("Refresh complete in {elapsed}ms · latest PR/workspace revision is open");
            *state = replacement;
        }
        Err(error) => {
            state.status = format!("Refresh failed after {elapsed}ms: {error:#}");
        }
    }
    Ok(())
}

fn cursor_is_fold(state: &RevState) -> bool {
    matches!(
        state
            .render_cache
            .as_ref()
            .and_then(|cache| cache.rows.get(state.row_cursor))
            .map(|row| &row.kind),
        Some(RevRowKind::Source { line, .. }) if is_expandable_fold(line)
    )
}

fn is_expandable_fold(line: &DiffLine) -> bool {
    line.kind == LineKind::Meta
        && line.content.starts_with("··· ")
        && line.content.contains(" unchanged lines ···")
}

fn expand_fold(state: &mut RevState, all: bool) -> Result<()> {
    let visible_index = state
        .render_cache
        .as_ref()
        .and_then(|cache| cache.rows.get(state.row_cursor))
        .and_then(|row| match &row.kind {
            RevRowKind::Source {
                visible_index,
                line,
            } if is_expandable_fold(line) => Some(*visible_index),
            _ => None,
        })
        .context("move onto an unchanged-lines fold before expanding it")?;
    let (repo_index, file_index) = state.current_indices().context("no current file")?;
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    let mut offset = 0usize;
    let hunk_index = file
        .hunks
        .iter()
        .position(|hunk| {
            let contains = (offset..offset + hunk.lines.len()).contains(&visible_index);
            offset += hunk.lines.len();
            contains
        })
        .context("fold row is not part of a hunk")?;
    let Some(next_hunk) = file.hunks.get(hunk_index + 1) else {
        state.status = "This fold has no following hunk to expand toward".into();
        return Ok(());
    };
    let current = &file.hunks[hunk_index];
    let old_start = current.old_start + current.old_count;
    let new_start = current.new_start + current.new_count;
    let hidden = next_hunk
        .old_start
        .saturating_sub(old_start)
        .min(next_hunk.new_start.saturating_sub(new_start));
    let count = if all { hidden } else { hidden.min(10) };
    if count == 0 {
        state.status = "No unchanged lines remain in this fold".into();
        return Ok(());
    }

    let repo_path = repo.record.path.clone();
    let working_root = repo
        .version
        .worktree_path
        .clone()
        .unwrap_or_else(|| repo_path.clone());
    let file_path = file.display_path.clone();
    let working = std::fs::read_to_string(working_root.join(&file_path)).or_else(|_| {
        Git::default().file_at_revision(&repo_path, &repo.version.head_sha, &file_path)
    })?;
    let working_lines = working.lines().collect::<Vec<_>>();
    let hunk = &mut state.workspace.repos[repo_index].diff.files[file_index].hunks[hunk_index];
    hunk.lines.retain(|line| !is_expandable_fold(line));
    for index in 0..count {
        hunk.lines.push(DiffLine {
            kind: LineKind::Context,
            old_line: Some(old_start + index),
            new_line: Some(new_start + index),
            content: working_lines
                .get(new_start + index - 1)
                .copied()
                .unwrap_or_default()
                .to_owned(),
        });
    }
    hunk.old_count += count;
    hunk.new_count += count;
    merge_touching_hunks(&mut state.workspace.repos[repo_index].diff.files[file_index].hunks);
    state.invalidate_rows();
    state.status = if all {
        format!("Expanded all {count} unchanged lines in this fold")
    } else {
        format!("Expanded {count} unchanged lines · press o again or O for all")
    };
    Ok(())
}

fn expand_hunk_edge(state: &mut RevState, above: bool) -> Result<()> {
    const STEP: usize = 5;

    let Some(visible_index) = state.render_cache.as_ref().and_then(|cache| {
        cache.rows.get(state.row_cursor).map(|row| match row.kind {
            RevRowKind::Source { visible_index, .. } => visible_index,
            RevRowKind::Annotation {
                anchor_visible_index,
                ..
            } => anchor_visible_index,
        })
    }) else {
        state.status = "Render the current file before expanding its hunk".into();
        return Ok(());
    };
    let (repo_index, file_index) = state.current_indices().context("no current file")?;
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    if matches!(file.status, FileStatus::Added | FileStatus::Deleted) {
        state.status = "Added and deleted files already show their complete changed side".into();
        return Ok(());
    }
    let mut offset = 0;
    let hunk_index = file
        .hunks
        .iter()
        .position(|hunk| {
            let contains = visible_index < offset + hunk.lines.len();
            offset += hunk.lines.len();
            contains
        })
        .context("current row is not part of a hunk")?;
    let repo_path = repo.record.path.clone();
    let working_root = repo
        .version
        .worktree_path
        .clone()
        .unwrap_or_else(|| repo_path.clone());
    let old_revision = repo
        .record
        .remote_pr_url
        .as_deref()
        .and_then(|url| PrReference::parse(url).ok())
        .map(|reference| format!("refs/rq-tui/pr/{}/base", reference.number))
        .unwrap_or_else(|| repo.version.head_sha.clone());
    let file_path = file.display_path.clone();
    let working = std::fs::read_to_string(working_root.join(&file_path)).or_else(|_| {
        Git::default().file_at_revision(&repo_path, &repo.version.head_sha, &file_path)
    })?;
    let old = Git::default()
        .file_at_revision(
            &repo_path,
            &old_revision,
            file.old_path.as_deref().unwrap_or(&file_path),
        )
        .unwrap_or_else(|_| working.clone());
    let working_lines = working.lines().collect::<Vec<_>>();
    let old_lines = old.lines().collect::<Vec<_>>();
    let hunk = &mut state.workspace.repos[repo_index].diff.files[file_index].hunks[hunk_index];
    let inserted = if above {
        let count = STEP
            .min(hunk.old_start.saturating_sub(1))
            .min(hunk.new_start.saturating_sub(1));
        if count == 0 {
            0
        } else {
            let old_start = hunk.old_start - count;
            let new_start = hunk.new_start - count;
            let mut lines = (0..count)
                .map(|index| DiffLine {
                    kind: LineKind::Context,
                    old_line: Some(old_start + index),
                    new_line: Some(new_start + index),
                    content: working_lines
                        .get(new_start + index - 1)
                        .copied()
                        .unwrap_or_default()
                        .to_owned(),
                })
                .collect::<Vec<_>>();
            lines.append(&mut hunk.lines);
            hunk.lines = lines;
            hunk.old_start = old_start;
            hunk.new_start = new_start;
            hunk.old_count += count;
            hunk.new_count += count;
            count
        }
    } else {
        let old_next = hunk.old_start + hunk.old_count;
        let new_next = hunk.new_start + hunk.new_count;
        let old_remaining = old_lines.len().saturating_sub(old_next.saturating_sub(1));
        let new_remaining = working_lines
            .len()
            .saturating_sub(new_next.saturating_sub(1));
        let count = STEP.min(old_remaining).min(new_remaining);
        for index in 0..count {
            hunk.lines.push(DiffLine {
                kind: LineKind::Context,
                old_line: Some(old_next + index),
                new_line: Some(new_next + index),
                content: working_lines
                    .get(new_next + index - 1)
                    .copied()
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        hunk.old_count += count;
        hunk.new_count += count;
        count
    };
    merge_touching_hunks(&mut state.workspace.repos[repo_index].diff.files[file_index].hunks);
    state.invalidate_rows();
    state.status = if inserted == 0 {
        if above {
            "No more unchanged lines above this hunk".into()
        } else {
            "No more unchanged lines below this hunk".into()
        }
    } else {
        format!(
            "Revealed {inserted} unchanged lines {} this hunk · gray rows are outside the diff",
            if above { "above" } else { "below" }
        )
    };
    Ok(())
}

fn merge_touching_hunks(hunks: &mut Vec<Hunk>) {
    for hunk in hunks.iter_mut() {
        hunk.lines.retain(|line| !is_expandable_fold(line));
    }
    let mut merged: Vec<Hunk> = Vec::with_capacity(hunks.len());
    for mut next in std::mem::take(hunks) {
        if let Some(previous) = merged.last_mut() {
            let old_touches = next.old_start <= previous.old_start + previous.old_count;
            let new_touches = next.new_start <= previous.new_start + previous.new_count;
            if old_touches && new_touches {
                let mut seen = previous
                    .lines
                    .iter()
                    .map(diff_line_identity)
                    .collect::<HashSet<_>>();
                next.lines
                    .retain(|line| seen.insert(diff_line_identity(line)));
                previous.lines.extend(next.lines);
                let old_end =
                    (previous.old_start + previous.old_count).max(next.old_start + next.old_count);
                let new_end =
                    (previous.new_start + previous.new_count).max(next.new_start + next.new_count);
                previous.old_count = old_end.saturating_sub(previous.old_start);
                previous.new_count = new_end.saturating_sub(previous.new_start);
                previous.header = format!(
                    "@@ -{},{} +{},{} @@",
                    previous.old_start, previous.old_count, previous.new_start, previous.new_count
                );
                continue;
            }
        }
        merged.push(next);
    }
    for index in 0..merged.len().saturating_sub(1) {
        let next_start = merged[index + 1].new_start;
        let current_end = merged[index].new_start + merged[index].new_count;
        let gap = next_start.saturating_sub(current_end);
        if gap > 0 {
            merged[index].lines.push(DiffLine {
                kind: LineKind::Meta,
                old_line: None,
                new_line: None,
                content: format!("··· {gap} unchanged lines ··· (o expand 10 · O expand all)"),
            });
        }
    }
    *hunks = merged;
}

fn diff_line_identity(line: &DiffLine) -> (u8, Option<usize>, Option<usize>) {
    let kind = match line.kind {
        LineKind::Context => 0,
        LineKind::Addition => 1,
        LineKind::Deletion => 2,
        LineKind::Meta => 3,
    };
    (kind, line.old_line, line.new_line)
}

fn context_keys(workspace: &ResolvedWorkItem) -> HashSet<String> {
    workspace
        .repos
        .iter()
        .flat_map(|repo| {
            repo.diff.files.iter().flat_map(move |file| {
                file.visible_lines()
                    .filter(|line| line.kind == LineKind::Context)
                    .map(move |line| context_key(&repo.record.id, file.path(), line))
            })
        })
        .collect()
}

fn context_key(repo_id: &str, path: &Path, line: &DiffLine) -> String {
    format!(
        "{repo_id}\0{}\0{:?}\0{:?}",
        path.display(),
        line.old_line,
        line.new_line
    )
}

fn review_file_key(repo_id: &str, path: &Path) -> String {
    format!("{repo_id}\0{}", path.display())
}

fn retract_selected_context(state: &mut RevState, highlighter: &mut dyn Highlighter) -> Result<()> {
    let (start, end) = selected_source_range(state, highlighter)
        .context("select source lines before retracting expanded context")?;
    let (repo_index, file_index) = state.current_indices().context("no current file")?;
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    let selected = file
        .visible_lines()
        .enumerate()
        .filter(|(index, line)| (start..=end).contains(index) && is_expanded_context(state, line))
        .map(|(_, line)| diff_line_identity(line))
        .collect::<HashSet<_>>();
    if selected.is_empty() {
        state.status =
            "The selection contains no gray expanded context · changed diff lines were retained"
                .into();
        return Ok(());
    }
    rebuild_file_without_expanded_lines(state, repo_index, file_index, &selected)?;
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    state.mode = RevMode::Normal;
    state.invalidate_rows();
    state.status = format!(
        "Retracted {} expanded context line(s) · changed and original diff lines were retained",
        selected.len()
    );
    Ok(())
}

fn rebuild_file_without_expanded_lines(
    state: &mut RevState,
    repo_index: usize,
    file_index: usize,
    removed: &HashSet<(u8, Option<usize>, Option<usize>)>,
) -> Result<()> {
    let repo = &state.workspace.repos[repo_index];
    let current = &repo.diff.files[file_index];
    let file_key = review_file_key(&repo.record.id, current.path());
    let mut rebuilt = state
        .original_files
        .get(&file_key)
        .cloned()
        .context("original diff snapshot is unavailable")?;
    let retained = current
        .visible_lines()
        .filter(|line| is_expanded_context(state, line))
        .filter(|line| !removed.contains(&diff_line_identity(line)))
        .cloned()
        .collect::<Vec<_>>();
    for line in retained {
        let old_start = line.old_line.unwrap_or(0);
        let new_start = line.new_line.unwrap_or(0);
        rebuilt.hunks.push(Hunk {
            header: format!("@@ -{old_start},1 +{new_start},1 @@"),
            old_start,
            old_count: usize::from(line.old_line.is_some()),
            new_start,
            new_count: usize::from(line.new_line.is_some()),
            lines: vec![line],
        });
    }
    rebuilt
        .hunks
        .sort_by_key(|hunk| (hunk.new_start, hunk.old_start));
    merge_touching_hunks(&mut rebuilt.hunks);
    state.workspace.repos[repo_index].diff.files[file_index] = rebuilt;
    Ok(())
}

fn retract_all_context(state: &mut RevState) -> Result<()> {
    let mut restored = 0usize;
    for repo in &mut state.workspace.repos {
        for file in &mut repo.diff.files {
            let key = review_file_key(&repo.record.id, file.path());
            let Some(original) = state.original_files.get(&key) else {
                continue;
            };
            if file != original {
                *file = original.clone();
                restored += 1;
            }
        }
    }
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    state.row_cursor = 0;
    state.review_scroll = 0;
    state.file_row_cursors.clear();
    state.file_review_scrolls.clear();
    state.invalidate_rows();
    state.status = if restored == 0 {
        "No expanded context is currently visible".into()
    } else {
        format!("Retracted expanded context in {restored} file(s)")
    };
    Ok(())
}

fn handle_compose_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            let editing_feedback =
                matches!(state.compose_target, Some(ComposeTarget::EditFeedback(_)));
            let retain_selection = state.visual_anchor.is_some()
                && (editing_feedback
                    || (state.compose.trim().is_empty()
                        && matches!(
                            state.compose_target,
                            Some(ComposeTarget::Feedback | ComposeTarget::NewQuestion)
                        )));
            state.mode = if retain_selection {
                RevMode::Visual
            } else {
                RevMode::Normal
            };
            state.compose_target = None;
            state.compose.clear();
            state.compose_cursor = 0;
            state.compose_scroll = 0;
            state.status = if editing_feedback {
                "Edit cancelled · original selection retained · Esc clears it".into()
            } else if retain_selection {
                "Empty box closed · selection retained · Esc clears it".into()
            } else {
                "Draft cancelled".into()
            };
            Ok(())
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.compose.insert(state.compose_cursor, '\n');
            state.compose_cursor += 1;
            state.compose_scroll = u16::MAX;
            Ok(())
        }
        KeyCode::Enter => {
            if state.compose.trim() == "/model" {
                let Some(ComposeTarget::FollowUp(annotation_id)) =
                    state.compose_target.as_ref().cloned()
                else {
                    state.status =
                        "/model is available while continuing an existing question thread".into();
                    return Ok(());
                };
                state.compose_target = None;
                state.compose.clear();
                state.compose_cursor = 0;
                state.compose_scroll = 0;
                state.mode = RevMode::Normal;
                start_model_switch(state, paths, &annotation_id);
                return Ok(());
            }
            submit_compose(state, storage, paths)
        }
        KeyCode::Backspace => {
            if state.compose_cursor > 0 {
                let previous = previous_grapheme_boundary(&state.compose, state.compose_cursor);
                state
                    .compose
                    .replace_range(previous..state.compose_cursor, "");
                state.compose_cursor = previous;
                state.compose_scroll = u16::MAX;
            }
            Ok(())
        }
        KeyCode::Delete => {
            let next = next_grapheme_boundary(&state.compose, state.compose_cursor);
            if next > state.compose_cursor {
                state.compose.replace_range(state.compose_cursor..next, "");
                state.compose_scroll = u16::MAX;
            }
            Ok(())
        }
        KeyCode::Left => {
            state.compose_cursor = previous_grapheme_boundary(&state.compose, state.compose_cursor);
            Ok(())
        }
        KeyCode::Right => {
            state.compose_cursor = next_grapheme_boundary(&state.compose, state.compose_cursor);
            Ok(())
        }
        KeyCode::Home => {
            state.compose_cursor = 0;
            state.compose_scroll = 0;
            Ok(())
        }
        KeyCode::End => {
            state.compose_cursor = state.compose.len();
            state.compose_scroll = u16::MAX;
            Ok(())
        }
        KeyCode::Up => {
            state.compose_scroll = state.compose_scroll.saturating_sub(1);
            Ok(())
        }
        KeyCode::Down => {
            state.compose_scroll = state.compose_scroll.saturating_add(1);
            Ok(())
        }
        KeyCode::PageUp => {
            state.compose_scroll = state.compose_scroll.saturating_sub(5);
            Ok(())
        }
        KeyCode::PageDown => {
            state.compose_scroll = state.compose_scroll.saturating_add(5);
            Ok(())
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.compose.insert(state.compose_cursor, character);
            state.compose_cursor += character.len_utf8();
            state.compose_scroll = u16::MAX;
            Ok(())
        }
        _ => Ok(()),
    }
}

fn submit_compose(state: &mut RevState, storage: &Storage, paths: &AppPaths) -> Result<()> {
    let text = state.compose.trim().to_owned();
    if text.is_empty() {
        state.status = "Enter some text before submitting".into();
        return Ok(());
    }
    if text == "/clear" {
        let annotation_id = match state.compose_target.as_ref() {
            Some(ComposeTarget::FollowUp(annotation_id)) => annotation_id.clone(),
            Some(ComposeTarget::NewQuestion) => {
                state.status = "There is no existing question thread to clear here".into();
                return Ok(());
            }
            _ => {
                state.status = "/clear is available only inside a question thread".into();
                return Ok(());
            }
        };
        if state
            .agent
            .as_ref()
            .is_some_and(|agent| agent.question_id == annotation_id)
            || state
                .queued_questions
                .iter()
                .any(|launch| launch.annotation_id == annotation_id)
        {
            state.status =
                "This thread is active or queued · Ctrl-C the active turn or let it finish first"
                    .into();
            return Ok(());
        }
        state.compose_target = None;
        state.compose.clear();
        state.compose_cursor = 0;
        state.compose_scroll = 0;
        state.mode = RevMode::Normal;
        return clear_question_thread(state, storage, &annotation_id);
    }
    if let Some(ComposeTarget::EditFeedback(annotation_id)) = state.compose_target.clone() {
        if let Err(error) = update_feedback(state, storage, &annotation_id, &text) {
            state.mode = RevMode::Compose;
            state.status = format!("Feedback save failed · draft and selection retained · {error}");
            return Ok(());
        }
        state.compose_target = None;
        state.compose.clear();
        state.compose_cursor = 0;
        state.compose_scroll = 0;
        return Ok(());
    }
    let target = state
        .compose_target
        .take()
        .context("composer has no target")?;
    state.compose.clear();
    state.compose_cursor = 0;
    state.compose_scroll = 0;
    state.mode = RevMode::Normal;
    match target {
        ComposeTarget::Feedback => create_feedback(state, storage, &text),
        ComposeTarget::EditFeedback(_) => unreachable!("feedback edits return before dispatch"),
        ComposeTarget::NewQuestion => create_question(state, storage, paths, &text),
        ComposeTarget::FollowUp(annotation_id) => {
            create_follow_up(state, storage, paths, &annotation_id, &text)
        }
    }
}

fn create_feedback(state: &mut RevState, storage: &Storage, text: &str) -> Result<()> {
    let (start, end) = selected_source_range_from_cache(state)?;
    let (repo_index, file_index) = state.current_indices().context("no current file")?;
    let repo = &state.workspace.repos[repo_index];
    let created = create_local_annotation(
        storage,
        &Git::default(),
        AnnotationRequest {
            repo: &repo.record,
            current_version: &repo.version,
            file: &repo.diff.files[file_index],
            selection_start: start,
            selection_end: end,
            kind: AnnotationKind::Comment,
            text: text.to_owned(),
        },
    )?;
    state.annotations.push((
        created.annotation,
        Placement {
            version_id: repo.version.id.clone(),
            ..created.placement
        },
    ));
    state.mode = RevMode::Normal;
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    state.invalidate_rows();
    state.status =
        "Feedback saved · e copies a structured prompt · `rev feedback` prints it".into();
    Ok(())
}

fn update_feedback(
    state: &mut RevState,
    storage: &Storage,
    annotation_id: &str,
    text: &str,
) -> Result<()> {
    let Some((annotation, _)) = state
        .annotations
        .iter_mut()
        .find(|(annotation, _)| annotation.id == annotation_id)
    else {
        anyhow::bail!("Feedback {annotation_id} no longer exists");
    };
    anyhow::ensure!(
        annotation.kind == AnnotationKind::Comment,
        "Only saved feedback can be edited"
    );
    storage.update_annotation_text(annotation_id, text)?;
    annotation.text = Some(text.to_owned());
    state.mode = RevMode::Normal;
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    state.invalidate_rows();
    state.status = "Feedback updated · e copies the refreshed structured prompt".into();
    Ok(())
}

fn clear_question_thread(
    state: &mut RevState,
    storage: &Storage,
    annotation_id: &str,
) -> Result<()> {
    storage.clear_rev_question_thread(annotation_id)?;
    state.threads.insert(annotation_id.to_owned(), Vec::new());
    state.question_sessions.remove(annotation_id);
    state.invalidate_rows();
    state.status =
        "Question thread cleared · press a to ask again with a fresh model and context".into();
    if state.agent.is_none() {
        state.agent_activity =
            "Cleared thread is detached from its previous Copilot session".into();
    }
    Ok(())
}

fn create_question(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    text: &str,
) -> Result<()> {
    let (start, end) = selected_source_range_from_cache(state)?;
    let (repo_index, file_index) = state.current_indices().context("no current file")?;
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    let created = create_local_annotation(
        storage,
        &Git::default(),
        AnnotationRequest {
            repo: &repo.record,
            current_version: &repo.version,
            file,
            selection_start: start,
            selection_end: end,
            kind: AnnotationKind::Ask,
            text: text.to_owned(),
        },
    )?;
    let user = created
        .ask_message
        .clone()
        .context("question annotation did not create a user message")?;
    let placement = Placement {
        version_id: repo.version.id.clone(),
        ..created.placement
    };
    let prompt = question_prompt(
        &repo.record.name,
        file,
        &placement,
        &created.annotation,
        text,
    );
    let annotation_id = created.annotation.id.clone();
    state.annotations.push((created.annotation, placement));
    state
        .threads
        .insert(annotation_id.clone(), vec![user.clone()]);
    state.invalidate_rows();
    queue_question_launch(
        state,
        paths,
        QuestionLaunch {
            annotation_id: annotation_id.clone(),
            pending: Some(PendingSend {
                annotation_id,
                user_message_id: user.id,
                assistant_message_id: Uuid::new_v4().to_string(),
                assistant_seq: 1,
                prompt,
                needs_model_picker: state.default_model.is_none(),
            }),
            existing: None,
        },
    );
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    Ok(())
}

fn create_follow_up(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    annotation_id: &str,
    text: &str,
) -> Result<()> {
    let existing = storage.rev_question_session(annotation_id)?;
    let seq = state
        .threads
        .get(annotation_id)
        .and_then(|thread| thread.last())
        .map(|message| message.seq + 1)
        .unwrap_or(0);
    let user = AskMessage {
        id: Uuid::new_v4().to_string(),
        annotation_id: annotation_id.to_owned(),
        seq,
        role: "user".into(),
        text: text.to_owned(),
        sent: false,
        delivery_state: DeliveryState::Pending,
        ts: now(),
    };
    let (prompt, needs_model_picker) = if existing.is_some() {
        (text.to_owned(), false)
    } else {
        let (annotation, placement) = state
            .annotation(annotation_id)
            .cloned()
            .context("this question thread no longer exists")?;
        let repo = state
            .workspace
            .repos
            .iter()
            .find(|repo| repo.record.id == annotation.repo_id)
            .context("question repository is no longer in this workspace")?;
        let file = repo
            .diff
            .files
            .iter()
            .find(|file| file.path() == annotation.file_path)
            .context("question file is no longer in the current diff")?;
        (
            question_prompt(&repo.record.name, file, &placement, &annotation, text),
            true,
        )
    };
    storage.append_rev_question_message(
        &user,
        existing.as_ref().map(|session| session.session_id.as_str()),
        existing.is_none(),
    )?;
    state
        .threads
        .entry(annotation_id.to_owned())
        .or_default()
        .push(user.clone());
    state.invalidate_rows();
    queue_question_launch(
        state,
        paths,
        QuestionLaunch {
            annotation_id: annotation_id.to_owned(),
            pending: Some(PendingSend {
                annotation_id: annotation_id.to_owned(),
                user_message_id: user.id,
                assistant_message_id: Uuid::new_v4().to_string(),
                assistant_seq: seq + 1,
                prompt,
                needs_model_picker,
            }),
            existing,
        },
    );
    Ok(())
}

fn question_prompt(
    repo_name: &str,
    file: &DiffFile,
    placement: &Placement,
    annotation: &Annotation,
    question: &str,
) -> String {
    format!(
        "Answer one isolated code-review question. Do not modify files.\n\
         Repository: {repo_name}\n\
         File: {}\n\
         Selected lines: {}-{} ({:?} side)\n\
         Selected code:\n```text\n{}\n```\n\
         Question: {question}",
        file.path().display(),
        placement.line_start,
        placement.line_end,
        placement.side,
        annotation.anchor_snippet,
    )
}

fn queue_question_launch(state: &mut RevState, paths: &AppPaths, launch: QuestionLaunch) {
    if state.agent.is_some() {
        state.queued_questions.push_back(launch);
        state.status = format!(
            "Question saved in its own session queue · {} waiting",
            state.queued_questions.len()
        );
        return;
    }
    start_question_agent(state, paths, launch);
}

fn start_question_agent(state: &mut RevState, paths: &AppPaths, launch: QuestionLaunch) {
    debug_assert!(state.agent.is_none());
    let model_switch_only = launch.pending.is_none();
    let needs_model_picker = launch
        .pending
        .as_ref()
        .is_none_or(|pending| pending.needs_model_picker);
    let selection = launch
        .existing
        .as_ref()
        .map(|session| ModelSelection {
            model_id: session.model_id.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
            context_tier: session.context_tier.clone(),
        })
        .or_else(|| state.default_model.clone())
        .unwrap_or_else(|| ModelSelection {
            model_id: "gpt-5".into(),
            reasoning_effort: None,
            context_tier: None,
        });
    let mut skill_directories = vec![paths.skills.clone()];
    let mut plugin_directories = vec![paths.plugins.clone()];
    for repo in &state.workspace.repos {
        let root = repo
            .version
            .worktree_path
            .as_deref()
            .unwrap_or(&repo.record.path);
        let skills = root.join(".rq-tui").join("skills");
        if skills.is_dir() {
            skill_directories.push(skills);
        }
        let plugins = root.join(".rq-tui").join("plugins");
        if plugins.is_dir() {
            plugin_directories.push(plugins);
        }
    }
    let runtime = start_agent(BridgeConfig {
        work_item_id: state.workspace.item.id.clone(),
        session_root: state.workspace.session_root.clone(),
        database_path: paths.database.clone(),
        app_paths: paths.clone(),
        existing_session_id: launch
            .existing
            .as_ref()
            .map(|session| session.session_id.clone()),
        persistent_session_active: false,
        model: selection.model_id.clone(),
        reasoning_effort: selection.reasoning_effort.clone(),
        context_tier: selection.context_tier.clone(),
        skill_directories,
        plugin_directories,
    });
    state.agent = Some(RevAgentSlot {
        question_id: launch.annotation_id,
        runtime,
        pending: launch.pending,
        outbound_id: None,
        session_id: launch
            .existing
            .as_ref()
            .map(|session| session.session_id.clone()),
        selection,
        needs_model_picker,
        model_switch_only,
    });
    state.agent_activity = if model_switch_only {
        "Resuming this question to load model choices…".into()
    } else if launch.existing.is_some() {
        "Resuming this question's Copilot session…".into()
    } else {
        "Creating a new Copilot session for this question…".into()
    };
    state.agent_last_event = Instant::now();
    state.status = if model_switch_only {
        "Opening this question's model picker · UI remains responsive".into()
    } else {
        "Question session starting in the background · input remains responsive".into()
    };
}

fn drain_agent_events(state: &mut RevState, storage: &Storage, paths: &AppPaths) -> Result<()> {
    for _ in 0..MAX_EVENTS_PER_TICK {
        let event = state
            .agent
            .as_ref()
            .and_then(|agent| agent.runtime.try_recv());
        let Some(event) = event else {
            break;
        };
        state.agent_last_event = Instant::now();
        if let Err(error) = handle_agent_event(state, storage, paths, event) {
            state.streaming = false;
            state.status = format!("Copilot event failed: {error}");
            state.agent_activity = "Copilot event failed; advancing the question queue".into();
            finish_active_question(state, paths);
            break;
        }
    }
    Ok(())
}

fn handle_agent_event(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    event: AgentEvent,
) -> Result<()> {
    match event {
        AgentEvent::SessionReady {
            session_id,
            resumed,
            resume_warning,
        } => {
            let agent = state
                .agent
                .as_mut()
                .context("session ready without agent")?;
            let expected_message_id = agent
                .pending
                .as_ref()
                .map(|pending| pending.user_message_id.clone());
            let expected_session_id = agent.session_id.clone();
            agent.session_id = Some(session_id.clone());
            let timestamp = now();
            let existing = storage.rev_question_session(&agent.question_id)?;
            let saved = RevQuestionSession {
                annotation_id: agent.question_id.clone(),
                session_id,
                model_id: agent.selection.model_id.clone(),
                reasoning_effort: agent.selection.reasoning_effort.clone(),
                context_tier: agent.selection.context_tier.clone(),
                state: "ready".into(),
                created_at: existing
                    .as_ref()
                    .map(|session| session.created_at.clone())
                    .unwrap_or_else(|| timestamp.clone()),
                updated_at: timestamp,
            };
            storage.upsert_rev_question_session(
                &saved,
                expected_message_id.as_deref(),
                expected_session_id.as_deref(),
            )?;
            state
                .question_sessions
                .insert(saved.annotation_id.clone(), saved);
            if agent.needs_model_picker {
                agent.runtime.send(AgentCommand::ListModels)?;
                state.agent_activity = "Loading models for this question…".into();
                state.status = "Question session ready · loading runtime model choices".into();
            } else if let Some(pending) = agent.pending.take() {
                send_pending(agent, pending)?;
                state.streaming = true;
                state.status = if resumed {
                    "Question session resumed · follow-up sent".into()
                } else {
                    "Question sent".into()
                };
            }
            if let Some(warning) = resume_warning {
                state.status = warning;
            }
        }
        AgentEvent::ModelsListed(models) => {
            if models.is_empty() {
                if send_with_startup_model(state)? {
                    state.status =
                        "No model choices were returned · question sent with the startup model"
                            .into();
                    state.agent_activity =
                        "Copilot is answering with the session's startup model".into();
                } else {
                    state.status =
                        "Copilot returned no selectable models · current thread is unchanged"
                            .into();
                    finish_active_question(state, paths);
                }
                return Ok(());
            }
            let initial_selection = state
                .agent
                .as_ref()
                .map(|agent| agent.selection.clone())
                .context("models listed without an active question")?;
            if !state
                .agent
                .as_ref()
                .is_some_and(|agent| agent.model_switch_only)
            {
                state.model_return_mode = RevMode::Normal;
            }
            state.picker = Some(ModelPicker::new(
                models,
                initial_selection,
                PickerScope::Thread,
            ));
            state.mode = RevMode::Model;
            state.status = "MODEL 1/3 · choose a model for this question".into();
        }
        AgentEvent::ModelsListFailed(message) => {
            let model_switch_only = state
                .agent
                .as_ref()
                .is_some_and(|agent| agent.model_switch_only);
            if !model_switch_only && send_with_startup_model(state)? {
                state.agent_activity =
                    format!("Model picker failed; answering with the startup model: {message}");
                state.status =
                    "Model choices unavailable · question sent with the session's startup model"
                        .into();
            } else {
                state.agent_activity = format!("Could not load Copilot models: {message}");
                state.status =
                    format!("Model picker failed: {message} · this thread keeps its current model");
                finish_active_question(state, paths);
            }
        }
        AgentEvent::ModelSelectionChanged(selection) => {
            let (question_id, session_id, expected_message_id, pending, model_switch_only) = {
                let agent = state
                    .agent
                    .as_mut()
                    .context("model changed without an active question")?;
                agent.selection = selection.clone();
                agent.needs_model_picker = false;
                (
                    agent.question_id.clone(),
                    agent
                        .session_id
                        .clone()
                        .context("model changed before session creation")?,
                    agent
                        .pending
                        .as_ref()
                        .map(|pending| pending.user_message_id.clone()),
                    agent.pending.take(),
                    agent.model_switch_only,
                )
            };
            let timestamp = now();
            let existing = storage.rev_question_session(&question_id)?;
            let saved = RevQuestionSession {
                annotation_id: question_id,
                session_id,
                model_id: selection.model_id.clone(),
                reasoning_effort: selection.reasoning_effort.clone(),
                context_tier: selection.context_tier.clone(),
                state: "ready".into(),
                created_at: existing
                    .as_ref()
                    .map(|session| session.created_at.clone())
                    .unwrap_or_else(|| timestamp.clone()),
                updated_at: timestamp,
            };
            storage.upsert_rev_question_session(
                &saved,
                expected_message_id.as_deref(),
                Some(&saved.session_id),
            )?;
            state
                .question_sessions
                .insert(saved.annotation_id.clone(), saved);
            if let Some(pending) = pending {
                let agent = state
                    .agent
                    .as_mut()
                    .context("selected model lost its question agent")?;
                send_pending(agent, pending)?;
                state.streaming = true;
                state.mode = RevMode::Normal;
                state.status = "Question sent in its own selected-model session".into();
            } else if model_switch_only {
                let return_mode = state.model_return_mode;
                state.agent.take();
                state.picker = None;
                state.mode = return_mode;
                state.agent_activity = format!("Question model changed to {}", selection.model_id);
                state.status = format!(
                    "Model changed to {} · future follow-ups use this selection",
                    selection.model_id
                );
            }
        }
        AgentEvent::ResponseStarted {
            outbound_id,
            first_delta,
            ..
        } => {
            let agent = state
                .agent
                .as_ref()
                .context("response started without an agent")?;
            if agent.outbound_id.as_deref() != Some(outbound_id.as_str()) {
                return Ok(());
            }
            let pending = pending_metadata(agent, &outbound_id, state)?;
            let response = AskMessage {
                id: pending.assistant_message_id,
                annotation_id: pending.annotation_id.clone(),
                seq: pending.assistant_seq,
                role: "assistant".into(),
                text: first_delta,
                sent: true,
                delivery_state: DeliveryState::Sent,
                ts: now(),
            };
            storage.acknowledge_ask_with_response_start(&pending.user_message_id, &response)?;
            state
                .threads
                .entry(pending.annotation_id)
                .or_default()
                .push(response);
            state.streaming = true;
            state.agent_activity = "Copilot is streaming an answer…".into();
            state.invalidate_rows();
        }
        AgentEvent::ResponseDelta { outbound_id, delta } => {
            if let Some((message_id, text)) = append_response(state, &outbound_id, &delta, false) {
                storage.update_ask_message_text(&message_id, &text)?;
            }
        }
        AgentEvent::ResponseSnapshot { outbound_id, text } => {
            if let Some((message_id, text)) = append_response(state, &outbound_id, &text, true) {
                storage.update_ask_message_text(&message_id, &text)?;
            }
        }
        AgentEvent::ResponseComplete {
            outbound_id,
            aborted,
        } => {
            if let Some((message_id, text)) = active_response(state, &outbound_id) {
                storage.update_ask_message_text(&message_id, &text)?;
            }
            settle_unsent_question(state, storage)?;
            state.streaming = false;
            state.agent_activity = if aborted {
                "Question cancelled".into()
            } else {
                "Answer complete".into()
            };
            state.status = state.agent_activity.clone();
            finish_active_question(state, paths);
        }
        AgentEvent::TurnFailed {
            outbound_id,
            message,
            ..
        } => {
            if let Some((message_id, text)) = active_response(state, &outbound_id) {
                storage.update_ask_message_text(&message_id, &text).ok();
            }
            settle_unsent_question(state, storage)?;
            state.streaming = false;
            state.agent_activity = format!("Question failed: {message}");
            state.status = state.agent_activity.clone();
            finish_active_question(state, paths);
        }
        AgentEvent::Activity { label, .. } => state.agent_activity = label,
        AgentEvent::Usage { model, .. } => {
            state.agent_activity = format!("Answer complete · model {model}");
        }
        AgentEvent::ModelSelectionFailed { message, .. } => {
            state.mode = RevMode::Model;
            state.status = format!("Model selection failed: {message}");
        }
        AgentEvent::Error(message) => {
            settle_unsent_question(state, storage)?;
            state.streaming = false;
            state.agent_activity = format!("Copilot error: {message}");
            state.status = state.agent_activity.clone();
            finish_active_question(state, paths);
        }
        AgentEvent::Stopped => {
            settle_unsent_question(state, storage)?;
            if state.streaming {
                state.streaming = false;
                state.status = "Copilot stopped before the answer completed".into();
            }
            finish_active_question(state, paths);
        }
        AgentEvent::StopSettledAlreadyIdle => {
            if state
                .agent
                .as_ref()
                .is_some_and(|agent| agent.needs_model_picker)
            {
                settle_unsent_question(state, storage)?;
                state.streaming = false;
                state.status =
                    "Question startup cancelled · unsent prompt retained as a draft".into();
                state.agent_activity = "Copilot cancellation settled while loading models".into();
                finish_active_question(state, paths);
            }
        }
        AgentEvent::HistoryLoaded(_)
        | AgentEvent::Queued { .. }
        | AgentEvent::QueueCancelled { .. }
        | AgentEvent::QueueReplaced { .. }
        | AgentEvent::QueueReplaceRejected { .. }
        | AgentEvent::SteeringAccepted { .. }
        | AgentEvent::SteeringFailed { .. }
        | AgentEvent::Forked { .. }
        | AgentEvent::ModelChanged(_)
        | AgentEvent::Compacted
        | AgentEvent::OrphanSideCleanup { .. }
        | AgentEvent::PruneSessionsStarted { .. }
        | AgentEvent::PruneSessionProgress { .. }
        | AgentEvent::PruneRecoveryStarted { .. }
        | AgentEvent::PruneSessionsComplete { .. }
        | AgentEvent::PruneRecovery { .. } => {}
    }
    Ok(())
}

fn settle_unsent_question(state: &mut RevState, storage: &Storage) -> Result<()> {
    let Some(pending) = state
        .agent
        .as_ref()
        .and_then(|agent| agent.pending.as_ref())
        .cloned()
    else {
        return Ok(());
    };
    let response_started = state
        .threads
        .get(&pending.annotation_id)
        .is_some_and(|thread| {
            thread
                .iter()
                .any(|message| message.id == pending.assistant_message_id)
        });
    if response_started {
        return Ok(());
    }
    storage.discard_pending_ask(&pending.user_message_id)?;
    if let Some(message) = state
        .threads
        .get_mut(&pending.annotation_id)
        .and_then(|thread| {
            thread
                .iter_mut()
                .find(|message| message.id == pending.user_message_id)
        })
    {
        message.delivery_state = DeliveryState::Draft;
        message.sent = false;
    }
    if let Some((annotation, _)) = state
        .annotations
        .iter_mut()
        .find(|(annotation, _)| annotation.id == pending.annotation_id)
    {
        annotation.delivery_state = DeliveryState::Draft;
    }
    Ok(())
}

fn send_with_startup_model(state: &mut RevState) -> Result<bool> {
    let Some(agent) = state.agent.as_mut() else {
        return Ok(false);
    };
    let Some(pending) = agent.pending.take() else {
        return Ok(false);
    };
    agent.needs_model_picker = false;
    send_pending(agent, pending)?;
    state.picker = None;
    state.mode = RevMode::Normal;
    state.streaming = true;
    Ok(true)
}

fn send_pending(agent: &mut RevAgentSlot, pending: PendingSend) -> Result<()> {
    let outbound = Outbound::new(
        OutboundKind::Ask {
            annotation_id: pending.annotation_id.clone(),
            user_message_id: pending.user_message_id.clone(),
            assistant_message_id: pending.assistant_message_id.clone(),
            assistant_seq: pending.assistant_seq,
        },
        pending.prompt,
    );
    agent.outbound_id = Some(outbound.id.clone());
    // Keep only the correlation metadata once the prompt enters the runtime.
    agent.pending = Some(PendingSend {
        prompt: String::new(),
        needs_model_picker: false,
        ..pending
    });
    agent.runtime.send(AgentCommand::Send(outbound))
}

fn pending_metadata(
    agent: &RevAgentSlot,
    outbound_id: &str,
    _state: &RevState,
) -> Result<PendingSend> {
    anyhow::ensure!(
        agent.outbound_id.as_deref() == Some(outbound_id),
        "response correlation does not belong to the active question"
    );
    agent
        .pending
        .clone()
        .context("active question lost its persistence metadata")
}

fn finish_active_question(state: &mut RevState, paths: &AppPaths) {
    let return_mode = if state
        .agent
        .as_ref()
        .is_some_and(|agent| agent.model_switch_only)
    {
        state.model_return_mode
    } else if state.mode == RevMode::Model {
        RevMode::Normal
    } else {
        state.mode
    };
    state.agent.take();
    state.picker = None;
    state.mode = return_mode;
    if let Some(next) = state.queued_questions.pop_front() {
        start_question_agent(state, paths, next);
    }
}

fn append_response(
    state: &mut RevState,
    outbound_id: &str,
    text: &str,
    snapshot: bool,
) -> Option<(String, String)> {
    let agent = state.agent.as_ref()?;
    if agent.outbound_id.as_deref() != Some(outbound_id) {
        return None;
    }
    let pending = agent.pending.as_ref()?;
    if let Some(message) = state
        .threads
        .get_mut(&pending.annotation_id)
        .and_then(|thread| {
            thread
                .iter_mut()
                .find(|message| message.id == pending.assistant_message_id)
        })
    {
        if snapshot {
            message.text.clear();
            message.text.push_str(text);
        } else {
            message.text.push_str(text);
        }
        let response = (message.id.clone(), message.text.clone());
        state.invalidate_rows();
        return Some(response);
    }
    None
}

fn active_response(state: &RevState, outbound_id: &str) -> Option<(String, String)> {
    let agent = state.agent.as_ref()?;
    if agent.outbound_id.as_deref() != Some(outbound_id) {
        return None;
    }
    let pending = agent.pending.as_ref()?;
    state
        .threads
        .get(&pending.annotation_id)?
        .iter()
        .find(|message| message.id == pending.assistant_message_id)
        .map(|message| (message.id.clone(), message.text.clone()))
}

fn handle_model_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    key: KeyEvent,
) -> Result<()> {
    let scope = state
        .picker
        .as_ref()
        .context("model mode has no picker")?
        .scope
        .clone();
    if key.code == KeyCode::Esc {
        let stage = state
            .picker
            .as_ref()
            .context("model mode has no picker")?
            .stage;
        if stage != PickerStage::Model {
            let picker = state.picker.as_mut().context("model mode has no picker")?;
            match stage {
                PickerStage::Context => {
                    picker.stage = PickerStage::Reasoning;
                    picker.index = picker
                        .reasoning_effort
                        .as_ref()
                        .and_then(|current| {
                            picker
                                .model()
                                .supported_reasoning_efforts
                                .iter()
                                .position(|effort| effort == current)
                        })
                        .map_or(0, |index| index + 1);
                    state.status = "MODEL 2/3 · choose thinking level · Esc goes back".into();
                }
                PickerStage::Reasoning => {
                    picker.stage = PickerStage::Model;
                    picker.index = picker
                        .filtered_model_indices()
                        .iter()
                        .position(|index| *index == picker.model_index)
                        .unwrap_or(0);
                    state.status = "MODEL 1/3 · type to search · ↑/↓ choose · Esc cancels".into();
                }
                PickerStage::Model => unreachable!(),
            }
            return Ok(());
        }
        if scope == PickerScope::GlobalDefault {
            state.mode = RevMode::Normal;
            state.picker = None;
            state.status = "Global model selection cancelled · the default is unchanged".into();
            return Ok(());
        }
        let model_switch_only = state
            .agent
            .as_ref()
            .is_some_and(|agent| agent.model_switch_only);
        if !model_switch_only {
            let startup_model = state
                .agent
                .as_ref()
                .map(|agent| agent.selection.model_id.clone())
                .unwrap_or_else(|| "runtime default".into());
            if send_with_startup_model(state)? {
                state.status = format!(
                    "Model picker closed · question sent with startup model {startup_model}"
                );
                state.agent_activity =
                    format!("Copilot is answering with startup model {startup_model}");
                return Ok(());
            }
        }
        if let Some(message_id) = state
            .agent
            .as_ref()
            .and_then(|agent| agent.pending.as_ref())
            .map(|pending| pending.user_message_id.clone())
        {
            storage.discard_pending_ask(&message_id)?;
        }
        state.mode = if model_switch_only {
            state.model_return_mode
        } else {
            RevMode::Normal
        };
        state.agent.take();
        state.picker = None;
        state.status = if model_switch_only {
            "Model change cancelled · the question keeps its previous model".into()
        } else {
            "Question kept as a draft; model selection cancelled".into()
        };
        if let Some(next) = state.queued_questions.pop_front() {
            start_question_agent(state, paths, next);
        }
        return Ok(());
    }

    let mut completed = None;
    {
        let picker = state.picker.as_mut().context("model mode has no picker")?;
        match key.code {
            KeyCode::Down => {
                picker.index = (picker.index + 1).min(picker.option_count().saturating_sub(1));
            }
            KeyCode::Char('j') if picker.stage != PickerStage::Model => {
                picker.index = (picker.index + 1).min(picker.option_count().saturating_sub(1));
            }
            KeyCode::Up => {
                picker.index = picker.index.saturating_sub(1);
            }
            KeyCode::Char('k') if picker.stage != PickerStage::Model => {
                picker.index = picker.index.saturating_sub(1);
            }
            KeyCode::Home => picker.index = 0,
            KeyCode::End => picker.index = picker.option_count().saturating_sub(1),
            KeyCode::Backspace if picker.stage == PickerStage::Model => {
                let previous = previous_grapheme_boundary(&picker.query, picker.query.len());
                picker.query.truncate(previous);
                picker.index = 0;
            }
            KeyCode::Char('u')
                if picker.stage == PickerStage::Model
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                picker.query.clear();
                picker.index = 0;
            }
            KeyCode::Char(character)
                if picker.stage == PickerStage::Model
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                picker.query.push(character);
                picker.index = 0;
            }
            KeyCode::Enter => match picker.stage {
                PickerStage::Model => {
                    let filtered = picker.filtered_model_indices();
                    let Some(model_index) = filtered.get(picker.index).copied() else {
                        state.status =
                            "No matching models · edit the search before continuing".into();
                        return Ok(());
                    };
                    picker.model_index = model_index;
                    picker.index = if picker.model().id == picker.initial_selection.model_id {
                        picker
                            .initial_selection
                            .reasoning_effort
                            .as_ref()
                            .and_then(|current| {
                                picker
                                    .model()
                                    .supported_reasoning_efforts
                                    .iter()
                                    .position(|effort| effort == current)
                            })
                            .map_or(0, |index| index + 1)
                    } else {
                        0
                    };
                    picker.stage = PickerStage::Reasoning;
                    state.status = "MODEL 2/3 · choose thinking level".into();
                }
                PickerStage::Reasoning => {
                    picker.reasoning_effort = picker
                        .index
                        .checked_sub(1)
                        .and_then(|index| picker.model().supported_reasoning_efforts.get(index))
                        .cloned();
                    picker.index = if picker.model().id == picker.initial_selection.model_id {
                        picker
                            .initial_selection
                            .context_tier
                            .as_ref()
                            .and_then(|current| {
                                picker
                                    .model()
                                    .context_tiers
                                    .iter()
                                    .position(|tier| &tier.id == current)
                            })
                            .map_or(0, |index| index + 1)
                    } else {
                        0
                    };
                    picker.stage = PickerStage::Context;
                    state.status = "MODEL 3/3 · choose context window".into();
                }
                PickerStage::Context => completed = Some(picker.selection()),
            },
            _ => {}
        }
    }

    if let Some(selection) = completed {
        if scope == PickerScope::GlobalDefault {
            storage.set_setting(
                REV_DEFAULT_MODEL_SETTING,
                &serde_json::to_string(&selection)?,
            )?;
            state.default_model = Some(selection.clone());
            state.picker = None;
            state.mode = RevMode::Normal;
            state.agent_activity = format!("Global default model is now {}", selection.model_id);
            state.status = format!(
                "Global default changed to {} · existing question threads keep their own models",
                selection.model_id
            );
        } else {
            let agent = state
                .agent
                .as_ref()
                .context("picker has no question agent")?;
            agent
                .runtime
                .send(AgentCommand::SelectModel(selection.clone()))?;
            state.status = format!("Applying {} to this question session…", selection.model_id);
        }
    }
    Ok(())
}

fn move_model_picker(state: &mut RevState, delta: isize) {
    let Some(picker) = state.picker.as_mut() else {
        return;
    };
    if delta >= 0 {
        picker.index = (picker.index + delta as usize).min(picker.option_count().saturating_sub(1));
    } else {
        picker.index = picker.index.saturating_sub(delta.unsigned_abs());
    }
}

fn begin_resolve(state: &mut RevState, annotation_id: String) {
    if state.agent.is_some() || !state.queued_questions.is_empty() {
        state.status =
            "Finish or cancel active and queued questions before resolving a review item".into();
        return;
    }
    let Some((annotation, placement)) = state.annotation(&annotation_id) else {
        state.status = "This review item no longer exists".into();
        return;
    };
    if annotation.status != AnnotationStatus::Active {
        state.status = "This review item is already inactive · use history to reopen it".into();
        return;
    }
    let status = format!(
        "Resolve this {} at {}:{}-{}? y/n",
        annotation.kind.as_str(),
        annotation.file_path.display(),
        placement.line_start,
        placement.line_end
    );
    state.pending_resolve = Some(annotation_id);
    state.mode = RevMode::ConfirmResolve;
    state.status = status;
}

fn resolve_pending_annotation(state: &mut RevState, storage: &Storage) -> Result<()> {
    let annotation_id = state
        .pending_resolve
        .take()
        .context("resolve confirmation lost its review item")?;
    let reason = "Resolved manually from the inline review";
    let changed_at =
        storage.set_annotation_status(&annotation_id, AnnotationStatus::Resolved, Some(reason))?;
    let annotation = state
        .annotations
        .iter_mut()
        .find(|(annotation, _)| annotation.id == annotation_id)
        .map(|(annotation, _)| annotation)
        .context("resolved review item disappeared from memory")?;
    annotation.status = AnnotationStatus::Resolved;
    annotation.status_reason = Some(reason.into());
    annotation.status_changed_at = Some(changed_at);
    state.mode = RevMode::Normal;
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    state.invalidate_rows();
    state.status =
        "Resolved · hidden inline and retained in history · r on source opens history".into();
    Ok(())
}

fn handle_history_key(state: &mut RevState, storage: &Storage, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('r') => {
            state.mode = RevMode::Normal;
            state.history_delete_armed = None;
            state.status = "Back to review".into();
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.history_cursor =
                (state.history_cursor + 1).min(state.annotations.len().saturating_sub(1));
            state.history_delete_armed = None;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.history_cursor = state.history_cursor.saturating_sub(1);
            state.history_delete_armed = None;
        }
        KeyCode::Char('u') => {
            if let Some((annotation, _)) = state.annotations.get_mut(state.history_cursor) {
                if annotation.status == AnnotationStatus::Active {
                    state.status = "This review item is already active".into();
                    return Ok(());
                }
                let changed_at = storage.set_annotation_status(
                    &annotation.id,
                    AnnotationStatus::Active,
                    Some("Auto-dismiss override: reopened manually from review history"),
                )?;
                annotation.status = AnnotationStatus::Active;
                annotation.status_reason =
                    Some("Auto-dismiss override: reopened manually from review history".into());
                annotation.status_changed_at = Some(changed_at);
                state.invalidate_rows();
                state.status =
                    "Reopened · item is active inline again at its latest saved placement".into();
            }
        }
        KeyCode::Char('d') => {
            if state.agent.is_some() || !state.queued_questions.is_empty() {
                state.status =
                    "Finish or cancel active and queued questions before deleting history".into();
                return Ok(());
            }
            if let Some((annotation, _)) = state.annotations.get(state.history_cursor).cloned() {
                if annotation.kind != AnnotationKind::Comment {
                    state.history_delete_armed = None;
                    state.status =
                        "d deletes saved feedback only · /clear resets an individual question thread"
                            .into();
                    return Ok(());
                }
                if state.history_delete_armed.as_deref() != Some(&annotation.id) {
                    state.history_delete_armed = Some(annotation.id);
                    state.status =
                        "Press d again to permanently delete this saved review item".into();
                    return Ok(());
                }
                storage.delete_annotation(&annotation.id)?;
                state.annotations.remove(state.history_cursor);
                state.threads.remove(&annotation.id);
                state.question_sessions.remove(&annotation.id);
                state.history_delete_armed = None;
                state.history_cursor = state
                    .history_cursor
                    .min(state.annotations.len().saturating_sub(1));
                state.invalidate_rows();
                state.status = "Deleted the selected persisted review item".into();
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_help_key(state: &mut RevState, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.mode = RevMode::Normal;
            state.status = "Back to review".into();
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.help_scroll = state.help_scroll.saturating_add(1)
        }
        KeyCode::Char('k') | KeyCode::Up => state.help_scroll = state.help_scroll.saturating_sub(1),
        KeyCode::PageDown => state.help_scroll = state.help_scroll.saturating_add(10),
        KeyCode::PageUp => state.help_scroll = state.help_scroll.saturating_sub(10),
        KeyCode::Char('g') | KeyCode::Home => state.help_scroll = 0,
        KeyCode::Char('G') | KeyCode::End => state.help_scroll = u16::MAX,
        KeyCode::Char(':') => {
            state.mode = RevMode::Command;
            state.command = CommandPalette::default();
            state.status = "COMMAND · type to filter · ↑/↓ select · Enter run · Esc return".into();
        }
        _ => {}
    }
}

fn copy_feedback_prompt(state: &mut RevState, storage: &Storage) -> Result<()> {
    let export = CommentExport::load(storage, &state.workspace.item)?;
    if export.comments.is_empty() {
        state.status = "No feedback to copy".into();
        return Ok(());
    }
    let prompt = export.structured_agent_prompt();
    let delivery = copy_to_clipboard(&prompt)?;
    state.status = format!(
        "Copied {} feedback item(s) {} · `rev feedback` also prints the prompt",
        export.comments.len(),
        clipboard_delivery_label(delivery)
    );
    Ok(())
}

fn clipboard_delivery_label(delivery: ClipboardDelivery) -> String {
    match delivery {
        ClipboardDelivery::Native(program) => format!("with {program}"),
        ClipboardDelivery::Osc52 => "via OSC 52 (terminal confirmation unavailable)".into(),
    }
}

fn yank_current_item(state: &mut RevState, highlighter: &mut dyn Highlighter) -> Result<()> {
    let text = current_item_yank_text(state, highlighter);
    state.pending_yank = false;
    if text.is_empty() {
        state.status = "Nothing copyable on this row".into();
        return Ok(());
    }
    let delivery = copy_to_clipboard(&text)?;
    state.status = format!(
        "Copied current item · {} character(s) {}",
        text.chars().count(),
        clipboard_delivery_label(delivery)
    );
    Ok(())
}

fn current_item_yank_text(state: &mut RevState, highlighter: &mut dyn Highlighter) -> String {
    let cursor = state.row_cursor;
    let width = cached_row_width(state);
    ensure_rows(state, width, highlighter)
        .get(cursor)
        .map(|row| row.item_text.trim_end().to_owned())
        .unwrap_or_default()
}

fn yank_visual_selection(state: &mut RevState, highlighter: &mut dyn Highlighter) -> Result<()> {
    let text = visual_selection_yank_text(state, highlighter);
    if text.is_empty() {
        state.status = "Nothing copyable in this selection".into();
        return Ok(());
    }
    let delivery = copy_to_clipboard(&text)?;
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    state.pending_yank = false;
    state.mode = RevMode::Normal;
    state.status = format!(
        "Copied selection · {} line(s) {}",
        text.lines().count(),
        clipboard_delivery_label(delivery)
    );
    Ok(())
}

fn visual_selection_yank_text(state: &mut RevState, highlighter: &mut dyn Highlighter) -> String {
    let width = cached_row_width(state);
    let rows = ensure_rows(state, width, highlighter).to_vec();
    if let Some(anchor) = state.visual_row_anchor {
        let start = anchor.min(state.row_cursor);
        let end = anchor.max(state.row_cursor);
        rows[start..=end]
            .iter()
            .map(|row| row.yank_text.trim_end())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else if let Some(anchor) = state.visual_anchor {
        let active = current_source_index_from_rows(&rows, state.row_cursor).unwrap_or(anchor);
        let start = anchor.min(active);
        let end = anchor.max(active);
        rows.iter()
            .filter_map(|row| match &row.kind {
                RevRowKind::Source {
                    visible_index,
                    line,
                } if (start..=end).contains(visible_index) => Some(line.content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    }
}

fn delete_contextual_comment(
    state: &mut RevState,
    storage: &Storage,
    highlighter: &mut dyn Highlighter,
) -> Result<()> {
    let id = match contextual_annotation(state, highlighter, AnnotationKind::Comment) {
        ContextualAnnotationMatch::Match(id) => id,
        ContextualAnnotationMatch::None => {
            state.history_delete_armed = None;
            state.status =
                "Move onto saved feedback or select its exact original lines before deleting"
                    .into();
            return Ok(());
        }
        ContextualAnnotationMatch::Blocked(message) => {
            state.history_delete_armed = None;
            state.status = format!("d deletes saved feedback only · {message}");
            return Ok(());
        }
    };
    if state.history_delete_armed.as_deref() != Some(&id) {
        state.history_delete_armed = Some(id);
        state.status = "DELETE FEEDBACK · press d again to permanently delete this comment".into();
        return Ok(());
    }
    storage.delete_annotation(&id)?;
    state
        .annotations
        .retain(|(annotation, _)| annotation.id != id);
    state.history_delete_armed = None;
    state.visual_anchor = None;
    state.visual_row_anchor = None;
    state.mode = RevMode::Normal;
    state.invalidate_rows();
    state.status = "Deleted saved feedback · questions were not affected".into();
    Ok(())
}

fn move_row(state: &mut RevState, highlighter: &mut dyn Highlighter, delta: isize) {
    let width = cached_row_width(state);
    let rows = ensure_rows(state, width, highlighter).to_vec();
    if rows.is_empty() {
        return;
    }
    let direction = delta.signum();
    let steps = delta.unsigned_abs().max(1);
    if state.mode == RevMode::Visual && state.visual_row_anchor.is_none() {
        let mut cursor = state.row_cursor;
        for _ in 0..steps {
            loop {
                let next = if direction >= 0 {
                    (cursor + 1).min(rows.len() - 1)
                } else {
                    cursor.saturating_sub(1)
                };
                if next == cursor {
                    break;
                }
                cursor = next;
                if matches!(rows[cursor].kind, RevRowKind::Source { .. }) {
                    break;
                }
            }
        }
        state.row_cursor = cursor;
    } else if delta >= 0 {
        state.row_cursor = (state.row_cursor + delta as usize).min(rows.len().saturating_sub(1));
    } else {
        state.row_cursor = state.row_cursor.saturating_sub(delta.unsigned_abs());
    }
    state.status = if state.mode == RevMode::Visual && state.visual_row_anchor.is_some() {
        let anchor = state.visual_row_anchor.unwrap_or(state.row_cursor);
        format!(
            "{} rendered row(s) selected · j/k extend through chat and code · y copy · Esc clear",
            state.row_cursor.abs_diff(anchor) + 1
        )
    } else if state.mode == RevMode::Visual {
        let active = current_source_index_from_rows(&rows, state.row_cursor)
            .or(state.visual_anchor)
            .unwrap_or(0);
        let anchor = state.visual_anchor.unwrap_or(active);
        format!(
            "{} source row(s) selected · j/k extend · a ask · c feedback · v/Esc clear",
            active.abs_diff(anchor) + 1
        )
    } else if state.row_cursor == 0 && delta < 0 {
        "Top of this file · press h for the previous file".into()
    } else if state.row_cursor + 1 == rows.len() && delta > 0 {
        "End of this file and its Q&A · press l for the next file".into()
    } else {
        "j/k stay inside this file".into()
    };
}

fn row_count(state: &mut RevState, highlighter: &mut dyn Highlighter) -> usize {
    let width = cached_row_width(state);
    ensure_rows(state, width, highlighter).len()
}

fn current_source_index(state: &mut RevState, highlighter: &mut dyn Highlighter) -> Option<usize> {
    let cursor = state.row_cursor;
    let width = cached_row_width(state);
    match &ensure_rows(state, width, highlighter).get(cursor)?.kind {
        RevRowKind::Source { visible_index, .. } => Some(*visible_index),
        RevRowKind::Annotation {
            anchor_visible_index,
            ..
        } => Some(*anchor_visible_index),
    }
}

fn current_annotation_id(
    state: &mut RevState,
    highlighter: &mut dyn Highlighter,
) -> Option<String> {
    let cursor = state.row_cursor;
    let width = cached_row_width(state);
    match &ensure_rows(state, width, highlighter).get(cursor)?.kind {
        RevRowKind::Annotation { annotation_id, .. } => Some(annotation_id.clone()),
        RevRowKind::Source { .. } => None,
    }
}

fn cached_row_width(state: &RevState) -> usize {
    state.render_cache.as_ref().map_or(100, |cache| cache.width)
}

fn selected_source_range(
    state: &mut RevState,
    highlighter: &mut dyn Highlighter,
) -> Option<(usize, usize)> {
    let active = current_source_index(state, highlighter)?;
    let anchor = state.visual_anchor.unwrap_or(active);
    Some((anchor.min(active), anchor.max(active)))
}

fn selected_source_range_from_cache(state: &mut RevState) -> Result<(usize, usize)> {
    let active = state
        .render_cache
        .as_ref()
        .and_then(|cache| cache.rows.get(state.row_cursor))
        .map(|row| match row.kind {
            RevRowKind::Source { visible_index, .. } => visible_index,
            RevRowKind::Annotation {
                anchor_visible_index,
                ..
            } => anchor_visible_index,
        })
        .context("current review row is unavailable")?;
    let anchor = state.visual_anchor.unwrap_or(active);
    Ok((anchor.min(active), anchor.max(active)))
}

fn ensure_rows<'a>(
    state: &'a mut RevState,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> &'a [RevRow] {
    let width = width.max(1);
    let rebuild = state
        .render_cache
        .as_ref()
        .is_none_or(|cache| cache.revision != state.rows_revision || cache.width != width);
    if rebuild {
        let rows = build_rows(state, width, highlighter);
        state.render_cache = Some(RenderCache {
            revision: state.rows_revision,
            width,
            rows,
        });
    }
    &state.render_cache.as_ref().expect("cache was built").rows
}

fn build_rows(state: &RevState, width: usize, highlighter: &mut dyn Highlighter) -> Vec<RevRow> {
    let Some((repo_index, file_index)) = state.current_indices() else {
        return Vec::new();
    };
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    let file_path = file.path().to_string_lossy();
    let mut annotations = state
        .annotations
        .iter()
        .filter(|(annotation, _)| {
            annotation.status == AnnotationStatus::Active
                && match annotation.kind {
                    AnnotationKind::Comment => state.show_comments,
                    AnnotationKind::Ask => state.show_questions,
                }
                && annotation.repo_id == repo.record.id
                && annotation.file_path.to_string_lossy() == file_path
        })
        .collect::<Vec<_>>();
    annotations.sort_by_key(|(_, placement)| (placement.line_end, placement.line_start));
    let mut rows = Vec::new();
    for (visible_index, line) in file.visible_lines().enumerate() {
        let rendered_lines = match state.diff_layout {
            RevDiffLayout::Unified => {
                source_lines(Some(file.path()), line, visible_index, width, highlighter)
            }
            RevDiffLayout::Split => {
                split_source_lines(Some(file.path()), line, visible_index, width, highlighter)
            }
        };
        for rendered_line in rendered_lines {
            rows.push(RevRow {
                kind: RevRowKind::Source {
                    visible_index,
                    line: line.clone(),
                },
                line: Some(rendered_line),
                yank_text: line.content.clone(),
                item_text: line.content.clone(),
            });
        }
        for (annotation, placement) in annotations.iter().filter(|(_, placement)| {
            let source_line = match placement.side {
                AnchorSide::Old => line.old_line,
                AnchorSide::New => line.new_line,
            };
            source_line.is_some_and(|source_line| placement.line_end == source_line as i64)
        }) {
            let annotation_item_text = if annotation.kind == AnnotationKind::Ask {
                state
                    .threads
                    .get(&annotation.id)
                    .and_then(|messages| messages.iter().find(|message| message.role == "user"))
                    .map(|message| message.text.clone())
                    .unwrap_or_default()
            } else {
                annotation.text.clone().unwrap_or_default()
            };
            let title = match annotation.kind {
                AnnotationKind::Comment => format!(
                    "Feedback · lines {}-{}",
                    placement.line_start, placement.line_end
                ),
                AnnotationKind::Ask => format!(
                    "Question · lines {}-{} · isolated session",
                    placement.line_start, placement.line_end
                ),
            };
            rows.push(RevRow {
                kind: RevRowKind::Annotation {
                    annotation_id: annotation.id.clone(),
                    anchor_visible_index: visible_index,
                },
                line: Some(Line::styled(
                    format!("╭─ {title}"),
                    Style::default()
                        .fg(if annotation.kind == AnnotationKind::Ask {
                            Color::Cyan
                        } else {
                            Color::Yellow
                        })
                        .add_modifier(Modifier::BOLD),
                )),
                yank_text: title,
                item_text: annotation_item_text.clone(),
            });
            if annotation.kind == AnnotationKind::Ask {
                for message in state.threads.get(&annotation.id).into_iter().flatten() {
                    let role = if message.role == "assistant" {
                        "Copilot"
                    } else {
                        "You"
                    };
                    rows.push(RevRow {
                        kind: RevRowKind::Annotation {
                            annotation_id: annotation.id.clone(),
                            anchor_visible_index: visible_index,
                        },
                        line: Some(Line::styled(
                            format!("│ {role}"),
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::BOLD),
                        )),
                        yank_text: role.to_owned(),
                        item_text: message.text.clone(),
                    });
                    for mapped in render_markdown_mapped(
                        &message.text,
                        width.saturating_sub(4).max(1),
                        highlighter,
                    )
                    .rows
                    {
                        let mut line = Line::from(vec![Span::raw("│ ")]);
                        line.spans.extend(mapped.line.spans);
                        let yank_text = line
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                            .trim_start_matches("│ ")
                            .to_owned();
                        rows.push(RevRow {
                            kind: RevRowKind::Annotation {
                                annotation_id: annotation.id.clone(),
                                anchor_visible_index: visible_index,
                            },
                            line: Some(line),
                            yank_text,
                            item_text: message.text.clone(),
                        });
                    }
                }
                rows.push(RevRow {
                    kind: RevRowKind::Annotation {
                        annotation_id: annotation.id.clone(),
                        anchor_visible_index: visible_index,
                    },
                    line: Some(Line::styled(
                        if state.threads.get(&annotation.id).is_some_and(Vec::is_empty)
                            && !state.question_sessions.contains_key(&annotation.id)
                        {
                            "╰─ cleared · a/i/Enter starts fresh · Q threads"
                        } else {
                            "╰─ a/i/Enter follow up · /clear resets · m model · Q threads"
                        },
                        Style::default().fg(Color::DarkGray),
                    )),
                    yank_text: String::new(),
                    item_text: annotation_item_text.clone(),
                });
            } else {
                let feedback = annotation.text.as_deref().unwrap_or_default();
                for mapped in
                    render_markdown_mapped(feedback, width.saturating_sub(4).max(1), highlighter)
                        .rows
                {
                    let mut line = Line::from(vec![Span::raw("│ ")]);
                    line.spans.extend(mapped.line.spans);
                    let yank_text = line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                        .trim_start_matches("│ ")
                        .to_owned();
                    rows.push(RevRow {
                        kind: RevRowKind::Annotation {
                            annotation_id: annotation.id.clone(),
                            anchor_visible_index: visible_index,
                        },
                        line: Some(line),
                        yank_text,
                        item_text: annotation_item_text.clone(),
                    });
                }
                rows.push(RevRow {
                    kind: RevRowKind::Annotation {
                        annotation_id: annotation.id.clone(),
                        anchor_visible_index: visible_index,
                    },
                    line: Some(Line::styled(
                        "╰─ saved · c/C edit · e copy structured prompt",
                        Style::default().fg(Color::DarkGray),
                    )),
                    yank_text: String::new(),
                    item_text: annotation_item_text,
                });
            }
        }
    }
    rows
}

fn render(frame: &mut Frame, state: &mut RevState, highlighter: &mut dyn Highlighter) {
    let area = frame.area();
    state.terminal_height = area.height as usize;
    if area.width < 36 || area.height < 9 {
        frame.render_widget(
            Paragraph::new("rev needs at least 36×9").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    }
    let compose_height = if state.mode == RevMode::Compose {
        composer_height(
            &state.compose,
            area.width.saturating_sub(4) as usize,
            area.height.saturating_sub(8),
        )
    } else {
        0
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(compose_height),
            Constraint::Length(3),
        ])
        .split(area);
    render_header(frame, state, vertical[0]);
    if state.mode == RevMode::History && !state.questions_open {
        render_history(frame, state, vertical[1]);
    } else if state.mode == RevMode::Help && !state.questions_open {
        render_help(frame, state, vertical[1]);
    } else {
        render_workspace_panes(frame, state, vertical[1], highlighter);
    }
    if state.mode == RevMode::Compose {
        render_composer(frame, state, vertical[2]);
    }
    render_footer(frame, state, vertical[3]);
    if state.mode == RevMode::Model {
        render_model_picker(frame, state, area);
    } else if state.mode == RevMode::Command {
        render_command_palette(frame, state, vertical[1]);
    } else if state.mode == RevMode::ConfirmClear {
        render_confirmation(
            frame,
            area,
            " confirm clear ",
            "Clear all persisted comments and questions for this workspace?",
        );
    } else if state.mode == RevMode::ConfirmResolve {
        render_confirmation(
            frame,
            area,
            " confirm resolve ",
            "Hide this item from the active review while retaining it in history?",
        );
    }
}

fn render_workspace_panes(
    frame: &mut Frame,
    state: &mut RevState,
    area: Rect,
    highlighter: &mut dyn Highlighter,
) {
    let tree = state.file_tree_open;
    let questions = state.questions_open;
    if tree && questions && area.width >= 108 {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(30),
                Constraint::Min(36),
                Constraint::Length(38),
            ])
            .split(area);
        render_file_picker(frame, state, panes[0]);
        render_review(frame, state, panes[1], highlighter);
        render_questions(frame, state, panes[2]);
    } else if questions && state.mode == RevMode::Questions {
        if area.width >= 72 {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(34), Constraint::Length(38)])
                .split(area);
            render_review(frame, state, panes[0], highlighter);
            render_questions(frame, state, panes[1]);
        } else {
            render_questions(frame, state, area);
        }
    } else if tree && area.width >= 68 {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(36)])
            .split(area);
        render_file_picker(frame, state, panes[0]);
        render_review(frame, state, panes[1], highlighter);
    } else if questions && area.width >= 72 {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(34), Constraint::Length(38)])
            .split(area);
        render_review(frame, state, panes[0], highlighter);
        render_questions(frame, state, panes[1]);
    } else if tree && state.mode == RevMode::FilePicker {
        render_file_picker(frame, state, area);
    } else {
        render_review(frame, state, area, highlighter);
    }
}

fn render_header(frame: &mut Frame, state: &RevState, area: Rect) {
    let repo = state
        .current_repo()
        .map_or("-", |repo| repo.record.name.as_str());
    let file = state
        .current_file()
        .map(|file| file.path().display().to_string())
        .unwrap_or_else(|| "no files".into());
    let title = format!(
        " rev · {} · {} > {} · {} · base {} · file {}/{} ",
        state.workspace.item.name,
        repo,
        file,
        state.diff_layout.label(),
        state
            .current_repo()
            .and_then(|repo| repo.record.base_branch.as_deref())
            .unwrap_or("auto"),
        state.file_index.saturating_add(1),
        state.files.len()
    );
    frame.render_widget(
        Paragraph::new(fit_text(&title, area.width as usize))
            .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn render_review(
    frame: &mut Frame,
    state: &mut RevState,
    area: Rect,
    highlighter: &mut dyn Highlighter,
) {
    let visual_mode = state.mode == RevMode::Visual;
    let editing_feedback = matches!(state.compose_target, Some(ComposeTarget::EditFeedback(_)));
    let selection_mode = visual_mode || editing_feedback;
    let border = if selection_mode {
        Color::Magenta
    } else {
        Color::Cyan
    };
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let width = inner.width.max(1) as usize;
    let rows = ensure_rows(state, width, highlighter).to_vec();
    state.clamp_cursor(rows.len());
    let viewport = inner.height.max(1) as usize;
    state.review_viewport_height = viewport;
    let max_start = rows.len().saturating_sub(viewport);
    state.review_scroll = state.review_scroll.min(max_start);
    if state.row_cursor < state.review_scroll {
        state.review_scroll = state.row_cursor;
    } else if state.row_cursor >= state.review_scroll.saturating_add(viewport) {
        state.review_scroll = state
            .row_cursor
            .saturating_add(1)
            .saturating_sub(viewport)
            .min(max_start);
    }
    let start = state.review_scroll;
    let source_selection = state
        .visual_anchor
        .zip(current_source_index_from_rows(&rows, state.row_cursor))
        .map(|(a, b)| (a.min(b), a.max(b)));
    let split_selection_side = source_selection.and_then(|range| {
        state
            .current_file()
            .and_then(|file| split_range_side(file, range))
    });
    let cursor_anchor_side = match rows.get(state.row_cursor).map(|row| &row.kind) {
        Some(RevRowKind::Annotation { annotation_id, .. }) => state
            .annotation(annotation_id)
            .map(|(_, placement)| placement.side),
        _ => None,
    };
    let row_selection = state
        .visual_row_anchor
        .map(|anchor| (anchor.min(state.row_cursor), anchor.max(state.row_cursor)));
    let cursor_source = current_source_index_from_rows(&rows, state.row_cursor);
    let title = if let Some((start, end)) = source_selection.filter(|_| editing_feedback) {
        format!(
            " EDIT FEEDBACK · {} original line(s) selected · Enter saves · Esc cancels ",
            end.saturating_sub(start) + 1
        )
    } else if let Some((start, end)) = row_selection.filter(|_| visual_mode) {
        format!(
            " VISUAL TEXT · {} rendered row(s) · j/k extend · y copy · v/Esc clear ",
            end.saturating_sub(start) + 1
        )
    } else if let Some((start, end)) = source_selection.filter(|_| visual_mode) {
        format!(
            " VISUAL LINE · {} selected · j/k extend · y copy · a ask · c feedback ",
            end.saturating_sub(start) + 1
        )
    } else {
        let hidden = match (state.show_comments, state.show_questions) {
            (true, true) => String::new(),
            (false, true) => " · comments hidden".into(),
            (true, false) => " · questions hidden".into(),
            (false, false) => " · annotations hidden".into(),
        };
        format!(
            " review · {}{} · j/k bounded · Ctrl-D/U half-page · h/l files · t files · v select ",
            state.diff_layout.label(),
            hidden
        )
    };
    frame.render_widget(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border)),
        area,
    );
    let mut visible = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(start).take(viewport) {
        let selected = index == state.row_cursor
            || matches!(
                &row.kind,
                RevRowKind::Source { visible_index, .. }
                    if cursor_source == Some(*visible_index)
            );
        let mut line = row.line.clone().unwrap_or_default();
        let expanded_context = matches!(
            &row.kind,
            RevRowKind::Source { line, .. } if is_expanded_context(state, line)
        );
        let visually_selected =
            match &row.kind {
                RevRowKind::Source { visible_index, .. } => source_selection
                    .is_some_and(|(start, end)| (start..=end).contains(visible_index)),
                RevRowKind::Annotation { .. } => false,
            } || row_selection.is_some_and(|(start, end)| (start..=end).contains(&index));
        if selected || visually_selected {
            let selection_color = if selection_mode {
                Color::Rgb(52, 35, 63)
            } else {
                Color::Rgb(35, 45, 58)
            };
            if state.diff_layout == RevDiffLayout::Split {
                if let RevRowKind::Source { line: source, .. } = &row.kind {
                    let side =
                        split_highlight_side(source, split_selection_side, cursor_anchor_side);
                    line = style_split_side(line, width, side, selection_color);
                } else {
                    line = line.style(Style::default().bg(selection_color));
                }
            } else {
                line = line.style(Style::default().bg(selection_color));
            }
        } else if expanded_context {
            line = line.style(Style::default().bg(Color::Rgb(38, 38, 38)));
        }
        visible.push(line);
    }
    frame.render_widget(Paragraph::new(Text::from(visible)), inner);
}

#[derive(Clone)]
struct MarkdownSides {
    old: String,
    new: String,
}

#[cfg(any())]
struct MarkdownRenderCache {
    key: String,
    rows: Vec<RenderedMarkdownRow>,
}

#[cfg(any())]
#[derive(Clone)]
struct RenderedMarkdownRow {
    line: Line<'static>,
    side: Option<AnchorSide>,
    source_line: Option<usize>,
}

fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "mdx"
            )
        })
}

fn markdown_sides(state: &RevState) -> Result<MarkdownSides> {
    let (repo_index, file_index) = state.current_indices().context("no current file")?;
    let repo = &state.workspace.repos[repo_index];
    let file = &repo.diff.files[file_index];
    if !is_markdown_path(file.path()) {
        anyhow::bail!("{} is not a Markdown file", file.path().display());
    }
    let new_root = repo
        .version
        .worktree_path
        .as_deref()
        .unwrap_or(&repo.record.path);
    let mut new = if file.status == FileStatus::Deleted {
        String::new()
    } else {
        std::fs::read_to_string(new_root.join(&file.display_path)).unwrap_or_default()
    };
    let old_revision = repo
        .record
        .remote_pr_url
        .as_deref()
        .and_then(|url| PrReference::parse(url).ok())
        .map(|reference| format!("refs/rq-tui/pr/{}/base", reference.number))
        .unwrap_or_else(|| repo.version.head_sha.clone());
    let old_path = file.old_path.as_deref().unwrap_or(&file.display_path);
    let mut old = if file.status == FileStatus::Added {
        String::new()
    } else {
        Git::default()
            .file_at_revision(&repo.record.path, &old_revision, old_path)
            .unwrap_or_default()
    };
    if new.is_empty() && file.status != FileStatus::Deleted {
        new = file
            .visible_lines()
            .filter(|line| !matches!(line.kind, LineKind::Deletion | LineKind::Meta))
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    if old.is_empty() && file.status != FileStatus::Added {
        old = file
            .visible_lines()
            .filter(|line| !matches!(line.kind, LineKind::Addition | LineKind::Meta))
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    Ok(MarkdownSides { old, new })
}

fn toggle_markdown_preview(state: &mut RevState, paths: &AppPaths) -> Result<()> {
    if state.markdown_preview {
        close_markdown_preview(state);
        return Ok(());
    }
    markdown_sides(state)?;
    state.markdown_preview = true;
    state.cmux_markdown_requested = true;
    state.markdown_sync_key = None;
    sync_markdown_preview(state, paths)?;
    state.status =
        "MARKDOWN RICH DIFF · opening browser preview · source-line sync stays live".into();
    Ok(())
}

fn close_markdown_preview(state: &mut RevState) {
    state.markdown_preview = false;
    state.cmux_markdown_requested = false;
    state.markdown_sync_key = None;
    state.markdown_content_key = None;
    state.markdown_sides_cache = None;
    if let Some(surface) = state.cmux_markdown.take() {
        state.cmux_close = Some(spawn_cmux_close(surface));
    }
    if state.cmux_open.is_none() {
        state.markdown_server = None;
    }
    state.status = "Rendered Markdown preview closed · cmux cleanup running in background".into();
}

fn spawn_cmux_close(surface: MarkdownSurface) -> Receiver<()> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        surface.close().ok();
        sender.send(()).ok();
    });
    receiver
}

fn drain_cmux_preview(state: &mut RevState) {
    if state
        .cmux_close
        .as_ref()
        .is_some_and(|receiver| !matches!(receiver.try_recv(), Err(TryRecvError::Empty)))
    {
        state.cmux_close = None;
        state.markdown_sync_key = None;
        if state.markdown_preview && state.cmux_markdown_requested {
            state.status =
                "Previous cmux preview closed · opening the requested replacement pane…".into();
        }
    }
    let Some(receiver) = state.cmux_open.as_ref() else {
        return;
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => {
            state.cmux_open = None;
            if state.markdown_preview {
                state.status =
                    "Terminal Markdown preview active · cmux opener stopped unexpectedly".into();
            }
            return;
        }
    };
    state.cmux_open = None;
    match result {
        Ok(Some(surface)) if state.markdown_preview => {
            state.cmux_markdown = Some(surface);
            state.status =
                "MARKDOWN RICH DIFF · browser opened · waiting for renderer heartbeat".into();
        }
        Ok(Some(surface)) => {
            state.cmux_close = Some(spawn_cmux_close(surface));
            state.markdown_server = None;
        }
        Ok(None) if state.markdown_preview => {
            state.markdown_server = None;
            state.status = "MARKDOWN RICH DIFF · browser opener unavailable".into();
        }
        Ok(None) => {}
        Err(error) if state.markdown_preview => {
            state.markdown_server = None;
            state.status = format!("Markdown rich diff unavailable: {error:#}");
        }
        Err(_) => {}
    }
    if !state.markdown_preview && state.cmux_open.is_none() {
        state.markdown_server = None;
    }
}

fn focused_diff_line(state: &RevState) -> Option<&DiffLine> {
    match &state
        .render_cache
        .as_ref()?
        .rows
        .get(state.row_cursor)?
        .kind
    {
        RevRowKind::Source { line, .. } => Some(line),
        RevRowKind::Annotation {
            anchor_visible_index,
            ..
        } => state
            .current_file()?
            .visible_lines()
            .nth(*anchor_visible_index),
    }
}

fn sync_markdown_preview(state: &mut RevState, _paths: &AppPaths) -> Result<()> {
    if !state.markdown_preview {
        return Ok(());
    }
    let Some(path) = state.current_file().map(|file| file.path().to_path_buf()) else {
        state.markdown_preview = false;
        state.cmux_markdown_requested = false;
        state.markdown_sync_key = None;
        state.markdown_content_key = None;
        state.markdown_sides_cache = None;
        if let Some(surface) = state.cmux_markdown.take() {
            state.cmux_close = Some(spawn_cmux_close(surface));
        }
        if state.cmux_open.is_none() {
            state.markdown_server = None;
        }
        state.status = "Markdown preview closed · refreshed revision has no changed files".into();
        return Ok(());
    };
    if !is_markdown_path(&path) {
        state.markdown_preview = false;
        state.cmux_markdown_requested = false;
        state.markdown_content_key = None;
        state.markdown_sides_cache = None;
        if let Some(surface) = state.cmux_markdown.take() {
            state.cmux_close = Some(spawn_cmux_close(surface));
        }
        if state.cmux_open.is_none() {
            state.markdown_server = None;
        }
        state.status =
            "Rendered Markdown preview closed because the selected file is not Markdown".into();
        return Ok(());
    }
    let (focus_side, focus) = focused_diff_line(state)
        .map(|line| {
            if line.kind == LineKind::Deletion {
                ("old", line.old_line.unwrap_or(1))
            } else {
                ("new", line.new_line.or(line.old_line).unwrap_or(1))
            }
        })
        .unwrap_or(("new", 1));
    let content_key = markdown_content_identity(state, &path);
    let content_changed = state.markdown_content_key.as_deref() != Some(&content_key);
    if content_changed {
        state.markdown_sides_cache = Some(markdown_sides(state)?);
        state.markdown_content_key = Some(content_key.clone());
    }
    update_markdown_connection_status(state);
    if state.cmux_markdown_requested && state.cmux_close.is_some() {
        state.status = "MARKDOWN DIFF · waiting for the previous cmux pane to close…".into();
        return Ok(());
    }
    let key = format!("{content_key}\0{focus_side}\0{focus}");
    if state.markdown_sync_key.as_deref() == Some(&key) {
        return Ok(());
    }
    state.markdown_sync_key = Some(key);
    if !state.cmux_markdown_requested {
        return Ok(());
    }
    if !MarkdownSurface::available() {
        return Ok(());
    }
    let sides = state
        .markdown_sides_cache
        .as_ref()
        .context("Markdown content cache was not initialized")?;
    let focus_state = browser_markdown_focus(&content_key, focus_side, focus)?;
    if let Some(server) = &state.markdown_server {
        if content_changed {
            server.update_document(browser_markdown_state(state, sides, &content_key)?);
        }
        server.update_focus(focus_state);
    } else {
        state.markdown_server = Some(MarkdownPreviewServer::start(
            browser_markdown_state(state, sides, &content_key)?,
            focus_state,
        )?);
    }
    if cfg!(test) {
        return Ok(());
    }
    if state.cmux_markdown.is_none() && state.cmux_open.is_none() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let url = state
            .markdown_server
            .as_ref()
            .context("Markdown browser server did not start")?
            .url();
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::SendError(Ok(Some(surface)))) =
                sender.send(MarkdownSurface::open(&url))
            {
                surface.close_detached().ok();
            }
        });
        state.cmux_open = Some(receiver);
    }
    Ok(())
}

fn update_markdown_connection_status(state: &mut RevState) {
    if state.cmux_markdown.is_none() {
        return;
    }
    let age = state
        .markdown_server
        .as_ref()
        .and_then(MarkdownPreviewServer::client_age);
    let waiting = state
        .markdown_server
        .as_ref()
        .map(MarkdownPreviewServer::waiting_for)
        .unwrap_or_default();
    if age.is_some_and(|age| age <= Duration::from_secs(2))
        && (state.status.contains("renderer heartbeat")
            || state.status.contains("browser preview disconnected")
            || state.status.contains("renderer has not connected"))
    {
        state.status =
            "MARKDOWN RICH DIFF · browser connected · following the active source line".into();
    } else if age.is_some_and(|age| age > Duration::from_secs(5))
        && state.status.starts_with("MARKDOWN RICH DIFF · browser")
    {
        state.status =
            "MARKDOWN RICH DIFF · browser disconnected · M closes; M again reopens".into();
    } else if age.is_none()
        && waiting > Duration::from_secs(5)
        && state.status.contains("renderer heartbeat")
    {
        state.status =
            "MARKDOWN RICH DIFF · renderer has not connected · inspect the browser pane".into();
    }
}

fn markdown_content_identity(state: &RevState, path: &Path) -> String {
    let file_stamp = state.current_repo().map_or_else(String::new, |repo| {
        let root = repo
            .version
            .worktree_path
            .as_deref()
            .unwrap_or(&repo.record.path);
        std::fs::metadata(root.join(path))
            .ok()
            .map(|metadata| format!("{}:{:?}", metadata.len(), metadata.modified().ok()))
            .unwrap_or_default()
    });
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}",
        state
            .current_repo()
            .map_or("", |repo| repo.version.id.as_str()),
        path.display(),
        state.annotations.len(),
        state.rows_revision,
        state.show_comments,
        state.show_questions,
        file_stamp
    )
}

fn browser_markdown_state(
    state: &RevState,
    sides: &MarkdownSides,
    revision: &str,
) -> Result<String> {
    let path = state
        .current_file()
        .map(|file| file.path().display().to_string())
        .unwrap_or_else(|| "Markdown".into());
    let mut additions = Vec::new();
    let mut deletions = Vec::new();
    if let Some(file) = state.current_file() {
        for line in file.visible_lines() {
            match line.kind {
                LineKind::Addition => additions.extend(line.new_line),
                LineKind::Deletion => deletions.extend(line.old_line),
                LineKind::Context | LineKind::Meta => {}
            }
        }
    }
    let mut notes = Vec::new();
    if let Some(repo) = state.current_repo() {
        if let Some(file) = state.current_file() {
            for (annotation, placement) in &state.annotations {
                if annotation.repo_id != repo.record.id
                    || annotation.file_path != file.display_path
                    || annotation.status != AnnotationStatus::Active
                    || match annotation.kind {
                        AnnotationKind::Comment => !state.show_comments,
                        AnnotationKind::Ask => !state.show_questions,
                    }
                {
                    continue;
                }
                let text = annotation.text.clone().or_else(|| {
                    state
                        .threads
                        .get(&annotation.id)
                        .and_then(|messages| messages.iter().find(|message| message.role == "user"))
                        .map(|message| message.text.clone())
                });
                notes.push(serde_json::json!({
                    "kind": annotation.kind.as_str(),
                    "side": match placement.side {
                        AnchorSide::Old => "old",
                        AnchorSide::New => "new",
                    },
                    "line_start": placement.line_start,
                    "line_end": placement.line_end,
                    "text": text.unwrap_or_else(|| "Question thread".into()),
                }));
            }
        }
    }
    Ok(serde_json::to_string(&serde_json::json!({
        "revision": revision,
        "path": path,
        "current": sides.new,
        "previous": sides.old,
        "additions": additions,
        "deletions": deletions,
        "notes": notes,
    }))?)
}

fn browser_markdown_focus(revision: &str, focus_side: &str, focus: usize) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "revision": revision,
        "focus_side": focus_side,
        "focus_line": focus,
    }))?)
}

#[cfg(any())]
fn render_markdown_diff_preview(
    frame: &mut Frame,
    state: &mut RevState,
    area: Rect,
    highlighter: &mut dyn Highlighter,
) {
    let title = if state.markdown_preview_focused {
        " Markdown focus · j/k · Ctrl-D/U · Tab source · -/+ diff "
    } else {
        " Markdown diff · Tab focus · Mermaid · - removed · + added "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if state.markdown_preview_focused {
            Color::Cyan
        } else {
            Color::Magenta
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if state.markdown_sides_cache.is_none() {
        let Ok(sides) = markdown_sides(state) else {
            frame.render_widget(Paragraph::new("Unable to load Markdown revisions"), inner);
            return;
        };
        state.markdown_sides_cache = Some(sides);
    }
    let render_key = format!(
        "{}:{}",
        state.markdown_content_key.as_deref().unwrap_or("snapshot"),
        inner.width
    );
    if state
        .markdown_render_cache
        .as_ref()
        .is_none_or(|cache| cache.key != render_key)
    {
        let sides = state
            .markdown_sides_cache
            .as_ref()
            .expect("Markdown sides were initialized");
        let rows =
            rendered_markdown_diff(&sides.old, &sides.new, inner.width as usize, highlighter);
        let rows = decorate_markdown_annotations(state, rows, inner.width as usize);
        state.markdown_render_cache = Some(MarkdownRenderCache {
            key: render_key,
            rows,
        });
    }
    let focus = focused_diff_line(state).and_then(|line| {
        line.new_line
            .map(|line| (AnchorSide::New, line))
            .or_else(|| line.old_line.map(|line| (AnchorSide::Old, line)))
    });
    let viewport = inner.height.max(1) as usize;
    state.markdown_viewport_height = viewport;
    let rows = &state
        .markdown_render_cache
        .as_ref()
        .expect("Markdown render cache was initialized")
        .rows;
    let focused_row = focus.and_then(|(side, line)| {
        rows.iter()
            .position(|row| row.side == Some(side) && row.source_line == Some(line))
    });
    let maximum = rows.len().saturating_sub(viewport);
    let start = if state.markdown_follow_source {
        focused_row
            .unwrap_or(state.markdown_scroll)
            .saturating_sub(viewport / 3)
            .min(maximum)
    } else {
        state.markdown_scroll.min(maximum)
    };
    state.markdown_scroll = start;
    let visible = rows
        .iter()
        .skip(start)
        .take(viewport)
        .map(|row| row.line.clone())
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), inner);
}

#[cfg(any())]
fn decorate_markdown_annotations(
    state: &RevState,
    rows: Vec<RenderedMarkdownRow>,
    width: usize,
) -> Vec<RenderedMarkdownRow> {
    let mut rendered = Vec::with_capacity(rows.len() + 8);
    let mut shown_annotations = HashSet::new();
    let current_repo_id = state.current_repo().map(|repo| repo.record.id.as_str());
    let current_path = state.current_file().map(|file| file.display_path.as_path());
    for row in rows {
        let side = row.side;
        let source_line = row.source_line;
        rendered.push(row);
        let (Some(side), Some(source_line), Some(repo_id), Some(path)) =
            (side, source_line, current_repo_id, current_path)
        else {
            continue;
        };
        for (annotation, placement) in &state.annotations {
            if annotation.status != AnnotationStatus::Active
                || annotation.repo_id != repo_id
                || annotation.file_path != path
                || placement.side != side
                || placement.line_end != source_line as i64
                || !shown_annotations.insert(annotation.id.clone())
            {
                continue;
            }
            let text = annotation.text.as_deref().unwrap_or_else(|| {
                state
                    .threads
                    .get(&annotation.id)
                    .and_then(|thread| thread.iter().find(|message| message.role == "user"))
                    .map_or("Question thread", |message| message.text.as_str())
            });
            rendered.push(RenderedMarkdownRow {
                line: Line::styled(
                    format!(
                        "╰─ {} L{}–{} · {}",
                        annotation.kind.as_str(),
                        placement.line_start,
                        placement.line_end,
                        fit_text(text, width.saturating_sub(18))
                    ),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                side: None,
                source_line: None,
            });
        }
    }
    if let (Some(repo_id), Some(path)) = (current_repo_id, current_path) {
        for (annotation, placement) in &state.annotations {
            if annotation.status != AnnotationStatus::Active
                || annotation.repo_id != repo_id
                || annotation.file_path != path
                || !shown_annotations.insert(annotation.id.clone())
            {
                continue;
            }
            rendered.push(RenderedMarkdownRow {
                line: Line::styled(
                    format!(
                        "╰─ {} L{}–{} · source-only anchor",
                        annotation.kind.as_str(),
                        placement.line_start,
                        placement.line_end
                    ),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                side: None,
                source_line: None,
            });
        }
    }
    rendered
}

#[cfg(any())]
fn rendered_markdown_diff(
    old_source: &str,
    new_source: &str,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<RenderedMarkdownRow> {
    let content_width = width.saturating_sub(2).max(1);
    let old = render_markdown_mapped(old_source, content_width, highlighter);
    let new = render_markdown_mapped(new_source, content_width, highlighter);
    let old_text = old.rows.iter().map(mapped_row_text).collect::<Vec<_>>();
    let new_text = new.rows.iter().map(mapped_row_text).collect::<Vec<_>>();
    let old_refs = old_text.iter().map(String::as_str).collect::<Vec<_>>();
    let new_refs = new_text.iter().map(String::as_str).collect::<Vec<_>>();
    let diff = TextDiff::from_slices(&old_refs, &new_refs);
    let mut rows = Vec::new();
    for change in diff.iter_all_changes() {
        let (line, side, source_line, style) = match change.tag() {
            ChangeTag::Delete => {
                let index = change.old_index().unwrap_or(0);
                (
                    prefixed_line("- ", old.rows[index].line.clone(), Color::Red),
                    Some(AnchorSide::Old),
                    mapped_row_source_line(&old.rows[index], old_source),
                    Style::default().bg(Color::Rgb(63, 30, 34)),
                )
            }
            ChangeTag::Insert => {
                let index = change.new_index().unwrap_or(0);
                (
                    prefixed_line("+ ", new.rows[index].line.clone(), Color::Green),
                    Some(AnchorSide::New),
                    mapped_row_source_line(&new.rows[index], new_source),
                    Style::default().bg(Color::Rgb(20, 58, 42)),
                )
            }
            ChangeTag::Equal => {
                let index = change.new_index().unwrap_or(0);
                (
                    prefixed_line("  ", new.rows[index].line.clone(), Color::DarkGray),
                    Some(AnchorSide::New),
                    mapped_row_source_line(&new.rows[index], new_source),
                    Style::default(),
                )
            }
        };
        rows.push(RenderedMarkdownRow {
            line: line.style(style),
            side,
            source_line,
        });
    }
    if old_source != new_source {
        rows.push(RenderedMarkdownRow {
            line: Line::styled(
                "── source/markup changes ──",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            side: None,
            source_line: None,
        });
        for change in TextDiff::from_lines(old_source, new_source).iter_all_changes() {
            let (marker, side, source_line, style) = match change.tag() {
                ChangeTag::Delete => (
                    "-",
                    Some(AnchorSide::Old),
                    change.old_index().map(|index| index + 1),
                    Style::default().bg(Color::Rgb(63, 30, 34)),
                ),
                ChangeTag::Insert => (
                    "+",
                    Some(AnchorSide::New),
                    change.new_index().map(|index| index + 1),
                    Style::default().bg(Color::Rgb(20, 58, 42)),
                ),
                ChangeTag::Equal => continue,
            };
            let content = vec![Span::raw(change.value().trim_end().to_owned())];
            for (index, wrapped) in wrap_spans(&content, width.saturating_sub(2).max(1))
                .into_iter()
                .enumerate()
            {
                let mut spans = vec![Span::styled(
                    if index == 0 {
                        format!("{marker} ")
                    } else {
                        "  ".into()
                    },
                    Style::default().fg(if marker == "+" {
                        Color::Green
                    } else {
                        Color::Red
                    }),
                )];
                spans.extend(wrapped);
                rows.push(RenderedMarkdownRow {
                    line: Line::from(spans).style(style),
                    side,
                    source_line,
                });
            }
        }
    }
    rows
}

#[cfg(any())]
fn prefixed_line(prefix: &'static str, line: Line<'static>, color: Color) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, Style::default().fg(color))];
    spans.extend(line.spans);
    Line::from(spans)
}

#[cfg(any())]
fn mapped_row_text(row: &MappedRow) -> String {
    row.line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[cfg(any())]
fn mapped_row_source_line(row: &MappedRow, source: &str) -> Option<usize> {
    let offset = row.cells.iter().find_map(|cell| match &cell.source {
        CellSource::Text(range) | CellSource::Decoration(range) => Some(range.start),
        CellSource::Synthetic => None,
    })?;
    Some(
        source[..offset.min(source.len())]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1,
    )
}

fn render_help(frame: &mut Frame, state: &mut RevState, area: Rect) {
    let block = Block::default()
        .title(" help · j/k/↑/↓ scroll · PgUp/PgDn · g/G · Esc close · q questions ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = help_lines();
    let width = inner.width.max(1) as usize;
    let total_rows = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum::<usize>();
    let visible_rows = inner.height.max(1) as usize;
    let max_scroll = total_rows.saturating_sub(visible_rows) as u16;
    state.help_scroll = state.help_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((state.help_scroll, 0)),
        inner,
    );
}

fn help_lines() -> Vec<Line<'static>> {
    let section = |title: &'static str| {
        Line::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    };
    let key = |binding: &'static str, description: &'static str| {
        Line::from(vec![
            Span::styled(
                format!("  {binding:<18}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(description),
        ])
    };
    vec![
        section("Review navigation"),
        key("j / ↓", "next logical row; wrapped source remains one item"),
        key(
            "k / ↑",
            "previous rendered row; stops at the start of this file",
        ),
        key("Ctrl-D / Ctrl-U", "move half a page down/up in the active pane"),
        key("h / ←", "previous file"),
        key("l / →", "next file"),
        key("g / Home", "first rendered row"),
        key("G / End", "last rendered row"),
        key(
            "mouse wheel",
            "scroll the active review, files, questions, composer, palette, history, or help",
        ),
        Line::raw(""),
        section("Diff and selection"),
        key("t", "open or close the repository/file tree"),
        key("v", "start or clear selection in code or rendered chat"),
        key("y", "copy a visual selection"),
        key(
            "yy",
            "copy the current source line or complete chat message",
        ),
        key("Esc", "clear the current selection"),
        key(
            "Tab (Markdown)",
            "focus preview for independent scrolling; Tab/Esc resumes source following",
        ),
        key(
            "Shift+↑",
            "reveal five unchanged lines above the active hunk",
        ),
        key(
            "Shift+↓",
            "reveal five unchanged lines below the active hunk",
        ),
        key("o", "reveal ten lines at the unchanged-lines fold"),
        key("O", "reveal every remaining line at the unchanged-lines fold"),
        key(
            "visual r",
            "retract selected gray expanded context; changed lines are retained",
        ),
        Line::raw(""),
        section("File tree"),
        key(
            "j/k / ↑/↓",
            "move through files and preview each one immediately",
        ),
        key("h/l / ←/→", "collapse or expand the selected repository"),
        key("g/G / Home/End", "jump to the first or last tree row"),
        key("Enter", "return to the previewed file"),
        key("t / Esc", "return to the previewed file"),
        Line::raw(""),
        section("Review actions"),
        key(
            "a",
            "ask on new lines; continue a question on its exact saved selection",
        ),
        key(
            "c",
            "save feedback on new lines; edit it on its exact saved selection",
        ),
        key("C", "explicitly edit feedback on its exact saved selection"),
        key("i / Enter", "follow up on the question under the cursor"),
        key("m", "switch the model for the question under the cursor"),
        key(
            "q / Q",
            "open or close the right-side question-thread panel",
        ),
        key("d d", "delete only the saved feedback under the cursor"),
        key(
            "r",
            "on an inline item, confirm resolve; on source, open history",
        ),
        key(
            "e",
            "copy the structured feedback prompt using native clipboard or OSC 52",
        ),
        key(
            "M",
            "toggle the synchronized browser rich diff with red/green gutters",
        ),
        key("y / n", "confirm or cancel clearing saved review history"),
        key(":", "open the command palette"),
        key(":q / :quit", "quit; plain q never exits"),
        Line::raw(""),
        section("Question and feedback composer"),
        key("Enter", "submit"),
        key("Shift+Enter", "insert a newline"),
        key("← / →", "move by terminal grapheme"),
        key("Home / End", "move to the start or end"),
        key("↑ / ↓", "scroll one row"),
        key("PgUp / PgDn", "scroll five rows"),
        key("Backspace / Del", "delete before or after the cursor"),
        key(
            "/clear",
            "inside a follow-up, clear only that thread and reset its context",
        ),
        key(
            "/model",
            "inside a follow-up, change only that question thread's model",
        ),
        key(
            "Esc",
            "cancel; an empty anchored box retains selection until Esc again",
        ),
        Line::raw(""),
        section("Copilot and model selection"),
        key(
            "Ctrl-C",
            "request cancellation of the active question from any mode",
        ),
        key("↑ / ↓", "move through model, thinking, and context options"),
        key("j / k", "also move on thinking and context stages"),
        key(
            "type / Backspace",
            "search runtime model IDs and names on the first picker stage",
        ),
        key("Enter", "choose one picker stage and continue to the next"),
        key(
            "Esc",
            "go back a stage; at models, existing threads cancel and new questions use the startup model",
        ),
        key(
            "queue",
            "later questions wait FIFO while the active one runs",
        ),
        Line::raw(""),
        section("Question threads"),
        key("j/k / ↑/↓", "move through saved question threads"),
        key("Enter", "jump to the selected inline thread"),
        key(
            "i / a",
            "continue the selected thread in the sticky composer",
        ),
        key("m", "choose a new model, thinking level, and context tier"),
        key("Q / Esc / q", "close the question panel"),
        Line::raw(""),
        section("History"),
        key(
            "j/k / ↑/↓ · Ctrl-D/U",
            "move one item or half a page down/up",
        ),
        key(
            "d, then d",
            "permanently delete selected saved feedback; questions are retained",
        ),
        key("u", "reopen a resolved or auto-dismissed item"),
        key("r / Esc", "return to the review"),
        key(
            "auto-dismiss",
            "new-side items hide only when selected code changes/disappears; moved, ambiguous, old-side, and manually reopened anchors stay active",
        ),
        Line::raw(""),
        section("Command palette"),
        key("type", "filter commands and branch completions"),
        key("↑ / ↓", "move through the scrollable results"),
        key("Tab", "complete the selected result"),
        key("Enter", "run the selected or typed command"),
        key("Esc", "close the palette"),
        Line::raw(""),
        section("Commands"),
        key(":help", "open this shortcut and command reference"),
        key(":diff unified", "render one full-width diff stream"),
        key(":diff split", "render old and new sides in two columns"),
        key(
            ":render markdown",
            "open one rich-diff browser view; cmux uses a native right pane",
        ),
        key(
            ":render markdown close",
            "close the rendered preview and only the cmux surface rev owns",
        ),
        key(":expand above", "reveal five lines above the active hunk"),
        key(":expand below", "reveal five lines below the active hunk"),
        key(":retract all", "retract expanded context in every file"),
        key(
            ":hide comments",
            "hide local feedback from the diff without deleting it",
        ),
        key(
            ":hide questions",
            "hide local question threads from the diff without deleting them",
        ),
        key(":show comments", "show hidden local feedback again"),
        key(":show questions", "show hidden local question threads again"),
        key(
            ":base <ref>",
            "rebuild the review relative to an autocompleted ref",
        ),
        key(":history", "open persisted review history"),
        key(
            ":refresh",
            "refresh local changes or fetch the latest GitHub PR revision in the background",
        ),
        key(":questions", "open the right-side question-thread panel"),
        key(":model", "set the global default for new question sessions"),
        key(":model reset", "ask for a model on each new question again"),
        key(":export feedback", "copy the structured feedback prompt"),
        key(":clear", "clear this workspace's saved review history"),
        key(":quit", "quit"),
        Line::raw(""),
        section("Non-interactive CLI"),
        key("rev history PATH", "print saved diff-related history"),
        key(
            "rev refresh PATH_OR_PR_URL",
            "refresh without entering the TUI",
        ),
        key("rev feedback PATH", "print the structured feedback prompt"),
        key(
            "rev export PATH",
            "write or print the structured feedback prompt",
        ),
        key("rev clear PATH", "remove local comments and Q&A"),
        key(
            "rev delete PATH",
            "permanently remove the workspace and review data",
        ),
    ]
}

fn is_expanded_context(state: &RevState, line: &DiffLine) -> bool {
    line.kind == LineKind::Context
        && state.current_repo().is_some_and(|repo| {
            state.current_file().is_some_and(|file| {
                !state
                    .original_context
                    .contains(&context_key(&repo.record.id, file.path(), line))
            })
        })
}

fn source_lines(
    path: Option<&Path>,
    line: &DiffLine,
    visible_index: usize,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<Line<'static>> {
    let marker = match line.kind {
        LineKind::Addition => "+",
        LineKind::Deletion => "-",
        LineKind::Context => " ",
        LineKind::Meta => "·",
    };
    let number = line.new_line.or(line.old_line).unwrap_or(0);
    let gutter_style = match line.kind {
        LineKind::Addition => Style::default().fg(Color::Green),
        LineKind::Deletion => Style::default().fg(Color::Red),
        LineKind::Context => Style::default().fg(Color::DarkGray),
        LineKind::Meta => Style::default().fg(Color::Blue),
    };
    let gutter = format!("{:>5} {marker} ", number);
    let rendered_gutter = if cell_width(&gutter) > width {
        fit_text(&gutter, width)
    } else {
        gutter
    };
    let available = width.saturating_sub(cell_width(&rendered_gutter));
    let content = highlighted_spans(path, line, visible_index, highlighter);
    let wrapped = wrap_spans(&content, available.max(1));
    let continuation = " ".repeat(cell_width(&rendered_gutter));
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, content)| {
            let mut spans = vec![if index == 0 {
                Span::styled(rendered_gutter.clone(), gutter_style)
            } else {
                Span::raw(continuation.clone())
            }];
            spans.extend(content);
            Line::from(spans)
        })
        .collect()
}

fn split_source_lines(
    path: Option<&Path>,
    line: &DiffLine,
    visible_index: usize,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<Line<'static>> {
    let left_width = width.saturating_sub(1) / 2;
    let right_width = width.saturating_sub(left_width + 1);
    let left = match line.kind {
        LineKind::Addition => vec![blank_side(left_width)],
        _ => source_side(
            path,
            line,
            visible_index,
            left_width,
            line.old_line,
            line.kind == LineKind::Deletion,
            highlighter,
        ),
    };
    let right = match line.kind {
        LineKind::Deletion => vec![blank_side(right_width)],
        _ => source_side(
            path,
            line,
            visible_index,
            right_width,
            line.new_line,
            line.kind == LineKind::Addition,
            highlighter,
        ),
    };
    let height = left.len().max(right.len()).max(1);
    (0..height)
        .map(|index| {
            let mut spans = left
                .get(index)
                .cloned()
                .unwrap_or_else(|| blank_side(left_width));
            pad_spans(&mut spans, left_width);
            spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
            let mut right = right
                .get(index)
                .cloned()
                .unwrap_or_else(|| blank_side(right_width));
            pad_spans(&mut right, right_width);
            spans.extend(right);
            Line::from(spans)
        })
        .collect()
}

fn blank_side(width: usize) -> Vec<Span<'static>> {
    vec![Span::raw(" ".repeat(width))]
}

fn source_side(
    path: Option<&Path>,
    line: &DiffLine,
    visible_index: usize,
    width: usize,
    number: Option<usize>,
    changed: bool,
    highlighter: &mut dyn Highlighter,
) -> Vec<Vec<Span<'static>>> {
    if width == 0 {
        return vec![Vec::new()];
    }
    let marker = if changed {
        match line.kind {
            LineKind::Addition => "+",
            LineKind::Deletion => "-",
            _ => " ",
        }
    } else {
        " "
    };
    let gutter = format!("{:>4} {marker} ", number.unwrap_or(0));
    let gutter_style = match line.kind {
        LineKind::Addition if changed => Style::default().fg(Color::Green),
        LineKind::Deletion if changed => Style::default().fg(Color::Red),
        LineKind::Meta => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::DarkGray),
    };
    let rendered_gutter = if cell_width(&gutter) > width {
        fit_text(&gutter, width)
    } else {
        gutter
    };
    let available = width.saturating_sub(cell_width(&rendered_gutter));
    let content = highlighted_spans(path, line, visible_index, highlighter);
    let wrapped = wrap_spans(&content, available.max(1));
    let continuation = " ".repeat(cell_width(&rendered_gutter));
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, content)| {
            let mut spans = vec![if index == 0 {
                Span::styled(rendered_gutter.clone(), gutter_style)
            } else {
                Span::raw(continuation.clone())
            }];
            spans.extend(content);
            pad_spans(&mut spans, width);
            spans
        })
        .collect()
}

fn highlighted_spans(
    path: Option<&Path>,
    line: &DiffLine,
    visible_index: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<Span<'static>> {
    path.and_then(|path| {
        highlighter
            .highlight_line(path, visible_index, &line.content)
            .ok()
    })
    .unwrap_or_else(|| {
        vec![StyledSegment {
            text: line.content.clone(),
            foreground: (210, 210, 210),
            bold: false,
            italic: false,
        }]
    })
    .into_iter()
    .map(|segment| {
        let mut style = Style::default().fg(Color::Rgb(
            segment.foreground.0,
            segment.foreground.1,
            segment.foreground.2,
        ));
        if segment.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if segment.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        Span::styled(segment.text, style)
    })
    .collect()
}

fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows = vec![Vec::new()];
    let mut used = 0usize;
    for span in spans {
        let mut chunk = String::new();
        for (_, grapheme) in grapheme_indices(span.content.as_ref()) {
            let grapheme_width = cell_width(grapheme);
            if used > 0 && used.saturating_add(grapheme_width) > width {
                if !chunk.is_empty() {
                    rows.last_mut()
                        .expect("wrapped rows are initialized")
                        .push(Span::styled(std::mem::take(&mut chunk), span.style));
                }
                rows.push(Vec::new());
                used = 0;
            }
            chunk.push_str(grapheme);
            used = used.saturating_add(grapheme_width);
        }
        if !chunk.is_empty() {
            rows.last_mut()
                .expect("wrapped rows are initialized")
                .push(Span::styled(chunk, span.style));
        }
    }
    rows
}

fn pad_spans(spans: &mut Vec<Span<'static>>, width: usize) {
    let current_width: usize = spans
        .iter()
        .map(|span| cell_width(span.content.as_ref()))
        .sum();
    if current_width < width {
        spans.push(Span::raw(" ".repeat(width - current_width)));
    }
}

fn split_line_side(line: &DiffLine) -> AnchorSide {
    if line.kind == LineKind::Deletion {
        AnchorSide::Old
    } else {
        AnchorSide::New
    }
}

fn split_highlight_side(
    line: &DiffLine,
    selection_side: Option<AnchorSide>,
    annotation_side: Option<AnchorSide>,
) -> AnchorSide {
    selection_side
        .or(annotation_side)
        .unwrap_or_else(|| split_line_side(line))
}

fn split_range_side(file: &DiffFile, range: (usize, usize)) -> Option<AnchorSide> {
    let mut additions = false;
    let mut deletions = false;
    for line in file
        .visible_lines()
        .skip(range.0)
        .take(range.1.saturating_sub(range.0) + 1)
    {
        additions |= line.kind == LineKind::Addition;
        deletions |= line.kind == LineKind::Deletion;
    }
    if additions && deletions {
        None
    } else if deletions {
        Some(AnchorSide::Old)
    } else {
        Some(AnchorSide::New)
    }
}

fn style_split_side(
    line: Line<'static>,
    width: usize,
    side: AnchorSide,
    background: Color,
) -> Line<'static> {
    let left_width = width.saturating_sub(1) / 2;
    let (start, end) = match side {
        AnchorSide::Old => (0, left_width),
        AnchorSide::New => (left_width.saturating_add(1), width),
    };
    let mut column = 0usize;
    let mut spans = Vec::new();
    for span in line.spans {
        let mut chunk = String::new();
        let mut chunk_selected = None;
        for (_, grapheme) in grapheme_indices(span.content.as_ref()) {
            let grapheme_width = cell_width(grapheme);
            let selected = column < end && column.saturating_add(grapheme_width) > start;
            if chunk_selected.is_some_and(|current| current != selected) {
                let style = if chunk_selected == Some(true) {
                    span.style.bg(background)
                } else {
                    span.style
                };
                spans.push(Span::styled(std::mem::take(&mut chunk), style));
            }
            chunk_selected = Some(selected);
            chunk.push_str(grapheme);
            column = column.saturating_add(grapheme_width);
        }
        if !chunk.is_empty() {
            let style = if chunk_selected == Some(true) {
                span.style.bg(background)
            } else {
                span.style
            };
            spans.push(Span::styled(chunk, style));
        }
    }
    Line::from(spans)
}

fn render_composer(frame: &mut Frame, state: &mut RevState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let mut text = state.compose.clone();
    let cursor = floor_grapheme_boundary(&text, state.compose_cursor);
    text.insert(cursor, '▏');
    let label = match state.compose_target {
        Some(ComposeTarget::Feedback) => " feedback ",
        Some(ComposeTarget::EditFeedback(_)) => " edit feedback ",
        Some(ComposeTarget::NewQuestion) => " question ",
        Some(ComposeTarget::FollowUp(_)) => " follow-up ",
        None => " input ",
    };
    let content_width = area.width.saturating_sub(2).max(1) as usize;
    let total_rows = wrapped_row_count(&text, content_width);
    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let max_scroll = total_rows.saturating_sub(visible_rows) as u16;
    state.compose_scroll = state.compose_scroll.min(max_scroll);
    let title = format!(
        "{label} · rows {}-{} of {} · ↑/↓ scroll ",
        usize::from(state.compose_scroll) + 1,
        (usize::from(state.compose_scroll) + visible_rows).min(total_rows),
        total_rows
    );
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((state.compose_scroll, 0))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Green)),
            ),
        area,
    );
}

fn render_footer(frame: &mut Frame, state: &RevState, area: Rect) {
    let mode = match state.mode {
        RevMode::Normal => "NORMAL",
        RevMode::Visual => "VISUAL",
        RevMode::Compose => "INSERT",
        RevMode::Command => "COMMAND",
        RevMode::Model => "MODEL",
        RevMode::History => "HISTORY",
        RevMode::FilePicker => "FILES",
        RevMode::Questions => "QUESTIONS",
        RevMode::Help => "HELP",
        RevMode::ConfirmClear | RevMode::ConfirmResolve => "CONFIRM",
    };
    let first = if area.width < 80 {
        format!("{mode} · {}", state.status)
    } else {
        format!(
            "{mode} · t files · v select · y/yy copy · q questions · a ask · c feedback · : commands · {}",
            state.status,
        )
    };
    let elapsed = state.agent_last_event.elapsed();
    let second = format!(
        "COPILOT · {} · last activity {}",
        state.agent_activity,
        format_duration(elapsed)
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(fit_text(&first, area.width as usize)),
            Line::styled(
                fit_text(&second, area.width as usize),
                Style::default().fg(if state.streaming {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }),
            ),
        ])
        .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn render_questions(frame: &mut Frame, state: &RevState, area: Rect) {
    let ids = question_ids(state);
    let selected = state.question_cursor.min(ids.len().saturating_sub(1));
    let block = Block::default()
        .title(" questions · j/k · ↵ · a continue · m model · q close ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if state.mode == RevMode::Questions {
            Color::Magenta
        } else {
            Color::DarkGray
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if ids.is_empty() {
        frame.render_widget(
            Paragraph::new("No question threads yet.\n\nSelect source lines and press a to ask.")
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let entry_height = 3usize;
    let visible_entries = (inner.height as usize / entry_height).max(1);
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible_entries)
        .min(ids.len().saturating_sub(visible_entries));
    let mut lines = Vec::new();
    for (index, id) in ids.iter().enumerate().skip(start).take(visible_entries) {
        let Some((annotation, placement)) = state.annotation(id) else {
            continue;
        };
        let selected_row = index == selected;
        let question = state
            .threads
            .get(id)
            .and_then(|thread| thread.iter().find(|message| message.role == "user"))
            .map(|message| message.text.as_str())
            .unwrap_or("Question");
        let turns = state.threads.get(id).map_or(0, |thread| {
            thread
                .iter()
                .filter(|message| message.role == "user")
                .count()
        });
        let model = state
            .question_sessions
            .get(id)
            .map(|session| session.model_id.as_str())
            .unwrap_or_else(|| {
                if state.threads.get(id).is_some_and(Vec::is_empty) {
                    "cleared · fresh on next ask"
                } else {
                    "session starting"
                }
            });
        let activity = if state
            .agent
            .as_ref()
            .is_some_and(|agent| agent.question_id == id.as_str())
        {
            if state.streaming {
                "answering"
            } else {
                "starting"
            }
        } else if let Some(position) = state
            .queued_questions
            .iter()
            .position(|launch| launch.annotation_id == id.as_str())
        {
            lines.push(Line::styled(
                fit_text(
                    &format!("{} {}", if selected_row { "❯" } else { " " }, question),
                    inner.width as usize,
                ),
                question_style(selected_row),
            ));
            lines.push(Line::styled(
                fit_text(
                    &format!(
                        "  {}:{} · {}",
                        annotation.file_path.display(),
                        placement.line_start,
                        model
                    ),
                    inner.width as usize,
                ),
                question_style(selected_row),
            ));
            lines.push(Line::styled(
                fit_text(
                    &format!("  queued #{} · {turns} turn(s)", position + 1),
                    inner.width as usize,
                ),
                question_style(selected_row),
            ));
            continue;
        } else {
            "ready"
        };
        let reasoning = state
            .question_sessions
            .get(id)
            .and_then(|session| session.reasoning_effort.as_deref())
            .map(|effort| format!(" · {effort}"))
            .unwrap_or_default();
        lines.push(Line::styled(
            fit_text(
                &format!("{} {}", if selected_row { "❯" } else { " " }, question),
                inner.width as usize,
            ),
            question_style(selected_row),
        ));
        lines.push(Line::styled(
            fit_text(
                &format!(
                    "  {}:{} · {model}{reasoning}",
                    annotation.file_path.display(),
                    placement.line_start
                ),
                inner.width as usize,
            ),
            question_style(selected_row),
        ));
        lines.push(Line::styled(
            fit_text(
                &format!("  {activity} · {turns} turn(s)"),
                inner.width as usize,
            ),
            question_style(selected_row),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn question_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(52, 35, 63))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn render_file_picker(frame: &mut Frame, state: &RevState, area: Rect) {
    let rows = file_picker_rows(state);
    let selected = state.file_picker_cursor.min(rows.len().saturating_sub(1));
    let viewport = area.height.saturating_sub(2).max(1) as usize;
    let start = selected
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(rows.len().saturating_sub(viewport));
    let end = (start + viewport).min(rows.len());
    let range = if rows.len() > viewport {
        format!(" · {}-{}/{}", start + 1, end, rows.len())
    } else {
        String::new()
    };
    let block = Block::default()
        .title(format!(
            " files{range} · j/k preview · h/l fold · ↵/t close "
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if state.mode == RevMode::FilePicker {
            Color::Cyan
        } else {
            Color::DarkGray
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(row_index, row)| {
            let selected_row = row_index == selected;
            let text = match *row {
                FilePickerRow::Repo(repo_index) => {
                    let repo = &state.workspace.repos[repo_index];
                    format!(
                        "{} {} {} ({} files)",
                        if selected_row { "❯" } else { " " },
                        if state.collapsed_repos.contains(&repo_index) {
                            "▸"
                        } else {
                            "▾"
                        },
                        repo.record.name,
                        repo.diff.files.len()
                    )
                }
                FilePickerRow::File {
                    repo_index,
                    flat_index,
                } => {
                    let (_, file_index) = state.files[flat_index];
                    let file = &state.workspace.repos[repo_index].diff.files[file_index];
                    let status = match file.status {
                        FileStatus::Added => "A",
                        FileStatus::Deleted => "D",
                        FileStatus::Renamed => "R",
                        FileStatus::Modified => "M",
                    };
                    format!(
                        "{}   {} {} {} +{} -{}",
                        if selected_row { "❯" } else { " " },
                        if flat_index == state.file_index {
                            "●"
                        } else {
                            " "
                        },
                        status,
                        file.path().display(),
                        file.additions,
                        file.deletions
                    )
                }
            };
            Line::styled(
                fit_text(&text, inner.width as usize),
                if selected_row {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(48, 48, 60))
                        .add_modifier(Modifier::BOLD)
                } else if matches!(row, FilePickerRow::Repo(_)) {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_command_palette(frame: &mut Frame, state: &RevState, area: Rect) {
    let width = area.width.saturating_sub(4).clamp(34, 84);
    let candidates = command_candidates(state);
    let visible_count = candidates.len().clamp(1, 8);
    let height = (visible_count as u16 + 3).min(area.height);
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let inner = Block::default()
        .title(" command palette · ↑/↓ scroll · Tab complete ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let body = inner.inner(popup);
    frame.render_widget(inner, popup);
    let mut input = state.command.input.clone();
    input.insert(floor_grapheme_boundary(&input, state.command.cursor), '▏');
    let start = state
        .command
        .selected
        .saturating_add(1)
        .saturating_sub(visible_count);
    let mut lines = vec![Line::styled(
        format!(
            ":{}",
            fit_text(&input, body.width.saturating_sub(1) as usize)
        ),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        candidates
            .iter()
            .enumerate()
            .skip(start)
            .take(visible_count)
            .map(|(index, candidate)| {
                Line::styled(
                    format!(
                        "{} {}",
                        if index == state.command.selected {
                            "❯"
                        } else {
                            " "
                        },
                        candidate
                    ),
                    if index == state.command.selected {
                        Style::default().bg(Color::Rgb(48, 48, 60))
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                )
            }),
    );
    frame.render_widget(Paragraph::new(lines), body);
}

fn render_model_picker(frame: &mut Frame, state: &RevState, area: Rect) {
    let Some(picker) = state.picker.as_ref() else {
        return;
    };
    let popup_width = if area.width < 50 {
        area.width
    } else {
        72.min(area.width.saturating_sub(2))
    };
    let popup = centered_rect(area, popup_width, 18.min(area.height));
    frame.render_widget(Clear, popup);
    let (title, options) = match picker.stage {
        PickerStage::Model => (
            if picker.scope == PickerScope::GlobalDefault {
                " global model 1/3 "
            } else {
                " question model 1/3 "
            },
            picker
                .filtered_model_indices()
                .iter()
                .map(|index| &picker.models[*index])
                .map(|model| {
                    let context = model
                        .max_context_tokens
                        .map(|tokens| format!(" · {tokens} ctx"))
                        .unwrap_or_default();
                    format!(
                        "{}{}{}",
                        model.name,
                        context,
                        if model.id == picker.initial_selection.model_id {
                            " · current"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>(),
        ),
        PickerStage::Reasoning => {
            let mut values = vec!["Runtime default".into()];
            values.extend(picker.model().supported_reasoning_efforts.clone());
            (" thinking 2/3 ", values)
        }
        PickerStage::Context => (
            " context 3/3 ",
            std::iter::once("Runtime default".into())
                .chain(picker.model().context_tiers.iter().map(|tier| {
                    tier.max_context_tokens
                        .map(|tokens| format!("{} · {tokens} tokens", tier.id))
                        .unwrap_or_else(|| tier.id.clone())
                }))
                .collect(),
        ),
    };
    let title = if popup.width < 60 {
        format!(
            " model {}/3 · ↵ next · Esc back ",
            match picker.stage {
                PickerStage::Model => 1,
                PickerStage::Reasoning => 2,
                PickerStage::Context => 3,
            }
        )
    } else {
        format!("{title} · ↑/↓ choose · Enter deeper · Esc back/cancel ")
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if picker.stage == PickerStage::Model {
            [Constraint::Min(1), Constraint::Length(3)]
        } else {
            [Constraint::Min(1), Constraint::Length(0)]
        })
        .split(inner);
    let list_area = sections[0];
    let viewport = list_area.height.max(1) as usize;
    let start = picker
        .index
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(options.len().saturating_sub(viewport));
    let lines = options
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, option)| {
            Line::styled(
                format!(
                    "{} {}",
                    if index == picker.index { "❯" } else { " " },
                    option
                ),
                if index == picker.index {
                    Style::default()
                        .fg(Color::Cyan)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(block, popup);
    if options.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching models · Backspace or Ctrl-U clears search")
                .style(Style::default().fg(Color::Yellow)),
            list_area,
        );
    } else {
        frame.render_widget(Paragraph::new(lines), list_area);
    }
    if picker.stage == PickerStage::Model {
        frame.render_widget(
            Paragraph::new(format!(
                "❯ {}",
                if picker.query.is_empty() {
                    "Search models…"
                } else {
                    picker.query.as_str()
                }
            ))
            .block(
                Block::default()
                    .title(format!(" search · {} match(es) ", options.len()))
                    .borders(Borders::TOP),
            ),
            sections[1],
        );
    }
}

fn render_history(frame: &mut Frame, state: &RevState, area: Rect) {
    let block = Block::default()
        .title(" persisted review history · u reopen · d d deletes feedback ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if inner.height >= 8 {
            [Constraint::Min(2), Constraint::Length(4)]
        } else {
            [Constraint::Min(1), Constraint::Length(0)]
        })
        .split(inner);
    let list_area = sections[0];
    let detail_area = sections[1];
    let viewport = list_area.height as usize;
    let start = state
        .history_cursor
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(state.annotations.len().saturating_sub(viewport));
    let rows = state
        .annotations
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, (annotation, placement))| {
            let lifecycle = match annotation.status {
                AnnotationStatus::Active => "active",
                AnnotationStatus::Resolved => "resolved",
                AnnotationStatus::AutoDismissed => "auto-dismissed",
            };
            let summary = if annotation.kind == AnnotationKind::Ask {
                state
                    .threads
                    .get(&annotation.id)
                    .and_then(|thread| thread.first())
                    .map(|message| message.text.as_str())
                    .unwrap_or("question")
            } else {
                annotation.text.as_deref().unwrap_or("feedback")
            };
            Line::styled(
                fit_text(
                    &format!(
                        "{} [{}] {}:{}-{} · {} · {}",
                        if index == state.history_cursor {
                            "❯"
                        } else {
                            " "
                        },
                        lifecycle,
                        annotation.file_path.display(),
                        placement.line_start,
                        placement.line_end,
                        annotation.kind.as_str(),
                        summary
                    ),
                    inner.width as usize,
                ),
                if index == state.history_cursor {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rows), list_area);
    if detail_area.height > 0 {
        let detail = state
            .annotations
            .get(state.history_cursor)
            .map(|(annotation, _)| {
                let changed = annotation
                    .status_changed_at
                    .as_deref()
                    .unwrap_or("not changed");
                let reason = annotation
                    .status_reason
                    .as_deref()
                    .unwrap_or("Active review item");
                format!(
                    "{} · {}\n{}\nu reopens inactive items · Esc returns",
                    annotation.status.as_str(),
                    changed,
                    reason
                )
            })
            .unwrap_or_else(|| "No saved review items".into());
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .block(Block::default().title(" lifecycle ").borders(Borders::TOP)),
            detail_area,
        );
    }
}

fn render_confirmation(frame: &mut Frame, area: Rect, title: &str, question: &str) {
    let width = 62.min(area.width);
    let height = (wrapped_row_count(question, width.saturating_sub(2) as usize) + 4)
        .try_into()
        .unwrap_or(u16::MAX);
    let popup = centered_rect(area, width, height.max(6).min(area.height.max(1)));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!("{question}\ny = confirm · n/Esc = cancel"))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            ),
        popup,
    );
}

fn current_source_index_from_rows(rows: &[RevRow], cursor: usize) -> Option<usize> {
    match &rows.get(cursor)?.kind {
        RevRowKind::Source { visible_index, .. } => Some(*visible_index),
        RevRowKind::Annotation {
            anchor_visible_index,
            ..
        } => Some(*anchor_visible_index),
    }
}

fn wrapped_row_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    text.split('\n')
        .map(|line| cell_width(line).max(1).div_ceil(width))
        .sum::<usize>()
}

fn composer_height(text: &str, width: usize, maximum: u16) -> u16 {
    let desired = wrapped_row_count(text, width).saturating_add(2) as u16;
    desired.clamp(3, maximum.max(3))
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn fit_text(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0usize;
    for (_, grapheme) in grapheme_indices(text) {
        let grapheme_width = cell_width(grapheme);
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        output.push_str(grapheme);
        used += grapheme_width;
    }
    output.push_str(&" ".repeat(width.saturating_sub(used)));
    output
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() < 1 {
        format!("{}ms ago", duration.as_millis())
    } else {
        format!("{}s ago", duration.as_secs())
    }
}

pub(crate) fn render_snapshot(width: u16, height: u16, snapshot: &str) -> Result<String> {
    let storage = Storage::in_memory()?;
    let workspace = snapshot_workspace(&storage)?;
    let mut state = RevState::load(workspace, &storage)?;
    match snapshot {
        "review" => {}
        "split" => state.diff_layout = RevDiffLayout::Split,
        "markdown" => {
            state.file_index = 1;
            state.markdown_preview = true;
            state.status =
                "MARKDOWN RICH DIFF · one browser document · red/green change gutters".into();
        }
        "expanded" => {
            let hunk = &mut state.workspace.repos[0].diff.files[0].hunks[0];
            let mut context = (5..10)
                .map(|line| DiffLine {
                    kind: LineKind::Context,
                    old_line: Some(line),
                    new_line: Some(line),
                    content: format!("unchanged line {line}"),
                })
                .collect::<Vec<_>>();
            context.append(&mut hunk.lines);
            hunk.lines = context;
            hunk.old_start = 5;
            hunk.new_start = 5;
            hunk.old_count += 5;
            hunk.new_count += 5;
            state.status = "Five gray unchanged lines revealed above the active hunk".into();
        }
        "command" => {
            state.mode = RevMode::Command;
            state.command.input = "diff".into();
            state.command.cursor = state.command.input.len();
        }
        "files" => open_file_picker(&mut state),
        "questions" => {
            seed_snapshot_questions(&mut state);
            open_questions(&mut state);
        }
        "panes" => {
            seed_snapshot_questions(&mut state);
            open_file_picker(&mut state);
            open_questions(&mut state);
        }
        "cleared-question" => {
            seed_snapshot_questions(&mut state);
            state
                .threads
                .insert("question-architecture".into(), Vec::new());
            state.question_sessions.remove("question-architecture");
            state.row_cursor = 2;
            state.status =
                "Question thread cleared · press a to ask again with a fresh model and context"
                    .into();
            state.agent_activity =
                "Cleared thread is detached from its previous Copilot session".into();
            state.invalidate_rows();
        }
        "edit-feedback" => {
            seed_snapshot_feedback(&mut state);
            state.visual_anchor = Some(0);
            state.row_cursor = 1;
            state.mode = RevMode::Compose;
            state.compose_target =
                Some(ComposeTarget::EditFeedback("feedback-architecture".into()));
            state.compose =
                "Keep isolated question sessions so follow-ups cannot leak context.".into();
            state.compose_cursor = state.compose.len();
            state.compose_scroll = u16::MAX;
            state.status =
                "EDIT FEEDBACK · original selection restored · Enter saves · Esc keeps selection"
                    .into();
        }
        "long-feedback" => {
            seed_snapshot_feedback(&mut state);
            state.annotations[0].0.text = Some(
                "This feedback is intentionally long enough to wrap across multiple terminal rows. \
                 Keep the complete explanation visible, including this final verification marker: \
                 FEEDBACK_TAIL."
                    .into(),
            );
            state.row_cursor = 1;
            state.status = "Saved feedback wraps without clipping".into();
            state.invalidate_rows();
        }
        "question-models" | "model-search" => {
            state.mode = RevMode::Model;
            state.status = "MODEL 1/3 · choose a model for this question".into();
            state.picker = Some(ModelPicker::new(
                vec![
                    ModelOption {
                        id: "gpt-5.2".into(),
                        name: "GPT-5.2".into(),
                        supported_reasoning_efforts: vec!["low".into(), "high".into()],
                        default_reasoning_effort: Some("high".into()),
                        max_context_tokens: Some(128_000),
                        context_tiers: vec![],
                    },
                    ModelOption {
                        id: "claude-sonnet-4.5".into(),
                        name: "Claude Sonnet 4.5".into(),
                        supported_reasoning_efforts: vec![],
                        default_reasoning_effort: None,
                        max_context_tokens: Some(200_000),
                        context_tiers: vec![],
                    },
                    ModelOption {
                        id: "gemini-3-pro".into(),
                        name: "Gemini 3 Pro".into(),
                        supported_reasoning_efforts: vec!["medium".into(), "high".into()],
                        default_reasoning_effort: Some("medium".into()),
                        max_context_tokens: Some(1_000_000),
                        context_tiers: vec![],
                    },
                ],
                ModelSelection {
                    model_id: "gpt-5.2".into(),
                    reasoning_effort: Some("high".into()),
                    context_tier: None,
                },
                PickerScope::Thread,
            ));
            if snapshot == "model-search" {
                state.picker.as_mut().unwrap().query = "claude".into();
                state.status =
                    "MODEL 1/3 · searching runtime models · ↑/↓ choose · Enter deeper".into();
            }
        }
        "visual" => {
            state.mode = RevMode::Visual;
            state.visual_anchor = Some(0);
            state.row_cursor = 2;
            state.status =
                "3 source row(s) selected · j/k extend · a ask · c feedback · v/Esc clear".into();
        }
        "composer" => {
            state.mode = RevMode::Compose;
            state.compose_target = Some(ComposeTarget::NewQuestion);
            state.compose = (1..=30)
                .map(|line| format!("Line {line}: a long question that exercises wrapping"))
                .collect::<Vec<_>>()
                .join("\n");
            state.compose_cursor = state.compose.len();
            state.compose_scroll = u16::MAX;
        }
        "resolve-confirm" => {
            seed_snapshot_feedback(&mut state);
            state.mode = RevMode::ConfirmResolve;
            state.pending_resolve = Some("feedback-architecture".into());
            state.status = "Resolve this feedback? y/n".into();
        }
        "resolved-history" | "stale-dismissed" => {
            seed_snapshot_feedback(&mut state);
            let annotation = &mut state.annotations[0].0;
            if snapshot == "resolved-history" {
                annotation.status = AnnotationStatus::Resolved;
                annotation.status_reason = Some("Resolved manually from the inline review".into());
            } else {
                annotation.status = AnnotationStatus::AutoDismissed;
                annotation.status_reason =
                    Some("Selected new-side code changed or disappeared".into());
            }
            annotation.status_changed_at = Some("2026-07-30T12:00:00Z".into());
            state.mode = RevMode::History;
        }
        "history" => state.mode = RevMode::History,
        "help" => {
            state.mode = RevMode::Help;
            state.help_scroll = 0;
            state.status = "j/k or arrows scroll · Esc closes · q opens questions".into();
        }
        "help-bottom" => {
            state.mode = RevMode::Help;
            state.help_scroll = u16::MAX;
            state.status = "j/k or arrows scroll · Esc closes · q opens questions".into();
        }
        "streaming" => {
            state.streaming = true;
            state.agent_activity =
                "tool · searching workspace · src/rev_ui.rs · event 12ms ago".into();
        }
        other => anyhow::bail!("unknown snapshot state: {other}"),
    }
    let mut highlighter = PlainHighlighter;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| render(frame, &mut state, &mut highlighter))?;
    Ok(terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn snapshot_workspace(storage: &Storage) -> Result<ResolvedWorkItem> {
    use crate::diff::parse_unified;
    use crate::domain::{BaseBranchSource, Repo, Version, VersionKind, WorkItem};

    let item = WorkItem {
        id: "rev-snapshot".into(),
        name: "workspace".into(),
        workspace_root: PathBuf::from("/workspace"),
        created_at: now(),
        updated_at: now(),
        last_opened_at: Some(now()),
    };
    storage.upsert_work_item(&item)?;
    let repo = Repo {
        id: "rev-snapshot-repo".into(),
        work_item_id: item.id.clone(),
        name: "demo".into(),
        path: PathBuf::from("/workspace/demo"),
        remote_pr_url: None,
        pr_meta_json: None,
        base_branch: Some("main".into()),
        base_branch_source: BaseBranchSource::Auto,
        last_activity_at: None,
    };
    storage.upsert_repo(&repo)?;
    let version = Version {
        id: "rev-snapshot-version".into(),
        repo_id: repo.id.clone(),
        version_num: 0,
        kind: VersionKind::WorkingTree,
        created_at: now(),
        head_sha: "head".into(),
        worktree_path: None,
        last_opened_at: Some(now()),
    };
    storage.upsert_version(&version)?;
    let diff = parse_unified(concat!(
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "--- a/src/lib.rs\n",
        "+++ b/src/lib.rs\n",
        "@@ -10,3 +10,4 @@\n",
        " fn review() {\n",
        "+    let isolated_questions = true;\n",
        "     finish();\n",
        " }\n",
        "diff --git a/README.md b/README.md\n",
        "--- a/README.md\n",
        "+++ b/README.md\n",
        "@@ -1,2 +1,3 @@\n",
        " # Demo\n",
        "+Use the file tree.\n",
        " Review changes.\n",
    ))?;
    Ok(ResolvedWorkItem {
        item,
        repos: vec![ReviewRepo {
            record: repo,
            version,
            diff,
        }],
        session_root: PathBuf::from("/workspace"),
    })
}

fn seed_snapshot_questions(state: &mut RevState) {
    let repo = &state.workspace.repos[0];
    let created_at = now();
    let questions = [
        (
            "question-architecture",
            PathBuf::from("src/lib.rs"),
            11,
            "Why should every review question use an isolated session?",
            "It keeps model choice, history, and follow-ups scoped to one thread.",
            Some(("gpt-5.2", Some("high"))),
        ),
        (
            "question-docs",
            PathBuf::from("README.md"),
            2,
            "Should the file-tree shortcut be documented here?",
            "Yes. Keep the discoverable shortcut next to bounded navigation.",
            Some(("claude-sonnet-4.5", None)),
        ),
    ];
    for (index, (id, file_path, line, question, answer, model)) in questions.into_iter().enumerate()
    {
        state.annotations.push((
            Annotation {
                id: id.into(),
                repo_id: repo.record.id.clone(),
                kind: AnnotationKind::Ask,
                file_path,
                anchor_snippet: question.into(),
                anchor_hash: format!("snapshot-{index}"),
                anchor_start_offset: 0,
                anchor_line_count: 1,
                text: None,
                submitted: true,
                delivery_state: DeliveryState::Sent,
                status: AnnotationStatus::Active,
                status_reason: None,
                status_changed_at: None,
                created_at: created_at.clone(),
            },
            Placement {
                annotation_id: id.into(),
                version_id: repo.version.id.clone(),
                side: AnchorSide::New,
                line_start: line,
                line_end: line,
                outdated: false,
                ambiguous: false,
            },
        ));
        state.threads.insert(
            id.into(),
            vec![
                AskMessage {
                    id: format!("{id}-user"),
                    annotation_id: id.into(),
                    seq: 0,
                    role: "user".into(),
                    text: question.into(),
                    sent: true,
                    delivery_state: DeliveryState::Sent,
                    ts: created_at.clone(),
                },
                AskMessage {
                    id: format!("{id}-assistant"),
                    annotation_id: id.into(),
                    seq: 1,
                    role: "assistant".into(),
                    text: answer.into(),
                    sent: true,
                    delivery_state: DeliveryState::Sent,
                    ts: created_at.clone(),
                },
            ],
        );
        if let Some((model_id, reasoning_effort)) = model {
            state.question_sessions.insert(
                id.into(),
                RevQuestionSession {
                    annotation_id: id.into(),
                    session_id: format!("session-{index}"),
                    model_id: model_id.into(),
                    reasoning_effort: reasoning_effort.map(str::to_owned),
                    context_tier: Some("standard".into()),
                    state: "ready".into(),
                    created_at: created_at.clone(),
                    updated_at: created_at.clone(),
                },
            );
        }
    }
    state.invalidate_rows();
}

fn seed_snapshot_feedback(state: &mut RevState) {
    let repo = &state.workspace.repos[0];
    state.annotations.push((
        Annotation {
            id: "feedback-architecture".into(),
            repo_id: repo.record.id.clone(),
            kind: AnnotationKind::Comment,
            file_path: PathBuf::from("src/lib.rs"),
            anchor_snippet: "fn review() {\n    let isolated_questions = true;".into(),
            anchor_hash: "snapshot-feedback".into(),
            anchor_start_offset: 0,
            anchor_line_count: 2,
            text: Some("Keep isolated question sessions so follow-ups cannot leak context.".into()),
            submitted: false,
            delivery_state: DeliveryState::Draft,
            status: AnnotationStatus::Active,
            status_reason: None,
            status_changed_at: None,
            created_at: now(),
        },
        Placement {
            annotation_id: "feedback-architecture".into(),
            version_id: repo.version.id.clone(),
            side: AnchorSide::New,
            line_start: 10,
            line_end: 11,
            outdated: false,
            ambiguous: false,
        },
    ));
    state.invalidate_rows();
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    use super::{
        browser_markdown_focus, browser_markdown_state, build_rows, close_file_picker,
        close_questions, current_item_yank_text, execute_command, finish_active_question,
        handle_agent_event, handle_compose_key, handle_file_picker_key, handle_help_key,
        handle_history_key, handle_key, handle_question_key, handle_review_key, help_lines,
        is_expandable_fold, markdown_sides, merge_touching_hunks, move_row, open_file_picker,
        open_questions, page_move, question_ids, queue_question_launch,
        rebuild_file_without_expanded_lines, render, render_snapshot, review_file_key, row_count,
        seed_snapshot_feedback, seed_snapshot_questions, snapshot_workspace, split_highlight_side,
        split_source_lines, style_split_side, visual_selection_yank_text, ComposeTarget,
        ModelPicker, PendingSend, PickerScope, PickerStage, QuestionLaunch, RevAgentSlot,
        RevDiffLayout, RevMode, RevRowKind, RevState,
    };
    use crate::config::AppPaths;
    use crate::copilot::{
        AgentCommand, AgentEvent, AgentRuntime, AgentSink, ModelOption, ModelSelection,
    };
    use crate::diff::{parse_unified, DiffLine, Hunk, LineKind};
    use crate::domain::{AnchorSide, AnnotationKind, AnnotationStatus};
    use crate::highlight::PlainHighlighter;
    use crate::storage::{RevQuestionSession, Storage};

    struct NoopAgent;

    impl AgentSink for NoopAgent {
        fn send(&self, _command: AgentCommand) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl AgentRuntime for NoopAgent {
        fn try_recv(&self) -> Option<AgentEvent> {
            None
        }
    }

    #[test]
    fn split_source_wraps_without_losing_the_tail_and_selection_owns_one_side() {
        let mut highlighter = PlainHighlighter;
        let line = DiffLine {
            kind: LineKind::Addition,
            old_line: None,
            new_line: Some(6),
            content: "AnchorSide, Annotation, AnnotationKind, AnnotationStatus, AskMessage, TAIL"
                .into(),
        };
        let wrapped = split_source_lines(None, &line, 0, 41, &mut highlighter);
        let rendered = wrapped
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wrapped.len() > 1);
        assert!(rendered.contains("TAIL"));
        assert!(!rendered.contains('…'));

        let selected = style_split_side(
            ratatui::text::Line::from("LLLL│RRRR"),
            9,
            AnchorSide::New,
            Color::Blue,
        );
        let cells = selected
            .spans
            .iter()
            .flat_map(|span| {
                span.content
                    .chars()
                    .map(move |character| (character, span.style.bg))
            })
            .collect::<Vec<_>>();
        assert!(cells[..5]
            .iter()
            .all(|(_, background)| background.is_none()));
        assert!(cells[5..]
            .iter()
            .all(|(_, background)| *background == Some(Color::Blue)));
        let context = DiffLine {
            kind: LineKind::Context,
            old_line: Some(6),
            new_line: Some(6),
            content: "unchanged".into(),
        };
        assert_eq!(
            split_highlight_side(&context, None, Some(AnchorSide::Old)),
            AnchorSide::Old,
            "an old-side context annotation keeps cursor ownership on the left"
        );
    }

    #[test]
    fn ctrl_d_and_ctrl_u_move_half_a_page_by_logical_rows() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let mut highlighter = PlainHighlighter;
        state.diff_layout = RevDiffLayout::Split;
        state.review_viewport_height = 8;
        page_move(&mut state, &mut highlighter, 1);
        let moved = state.row_cursor;
        assert!(moved > 0);
        page_move(&mut state, &mut highlighter, -1);
        assert_eq!(state.row_cursor, 0);
    }

    #[test]
    fn browser_markdown_state_keeps_diff_sides_source_lines_and_review_notes() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        state.file_index = 1;
        seed_snapshot_questions(&mut state);
        let sides = markdown_sides(&state).unwrap();
        let payload = serde_json::from_str::<serde_json::Value>(
            &browser_markdown_state(&state, &sides, "revision-7").unwrap(),
        )
        .unwrap();
        let focus = serde_json::from_str::<serde_json::Value>(
            &browser_markdown_focus("revision-7", "new", 2).unwrap(),
        )
        .unwrap();

        assert_eq!(payload["revision"], "revision-7");
        assert_eq!(payload["path"], "README.md");
        assert_eq!(focus["focus_side"], "new");
        assert_eq!(focus["focus_line"], 2);
        assert_eq!(payload["additions"], serde_json::json!([2]));
        assert_eq!(payload["deletions"], serde_json::json!([]));
        assert_eq!(payload["notes"][0]["side"], "new");
        assert!(payload["current"]
            .as_str()
            .unwrap()
            .contains("Use the file tree."));
        assert!(payload["notes"][0]["text"]
            .as_str()
            .unwrap()
            .contains("file-tree shortcut"));
    }

    #[test]
    fn markdown_snapshot_is_agent_inspectable() {
        let frame = render_snapshot(120, 30, "markdown").unwrap();
        assert!(frame.contains("MARKDOWN RICH DIFF"));
        assert!(frame.contains("Demo"));
        assert!(frame.contains("Use the file tree."));

        let narrow = render_snapshot(60, 20, "markdown").unwrap();
        assert!(narrow.contains("MARKDOWN RICH DIFF"));
        assert!(narrow.contains("Use the file tree."));
        assert!(narrow.contains("review · unified"));
    }

    fn test_paths(name: &str) -> AppPaths {
        let root = std::path::PathBuf::from("/tmp").join(name);
        AppPaths {
            data: root.join("data"),
            cache: root.join("cache"),
            database: root.join("rev.db"),
            roots: root.join("roots"),
            prs: root.join("prs"),
            exports: root.join("exports"),
            skills: root.join("skills"),
            plugins: root.join("plugins"),
        }
    }

    #[test]
    fn render_markdown_opens_only_the_browser_rich_diff() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        state.file_index = 1;
        execute_command(
            &mut state,
            &storage,
            &test_paths("rev-markdown-rich-diff"),
            "render markdown",
        )
        .unwrap();
        assert!(state.markdown_preview);
        assert!(state.cmux_markdown_requested);
        assert!(state.markdown_server.is_some());
        assert!(state.cmux_open.is_none());
        assert!(state.cmux_markdown.is_none());
    }

    #[test]
    fn snapshot_exposes_the_small_rev_surface() {
        let snapshot = render_snapshot(100, 24, "review").unwrap();
        assert!(snapshot.contains("rev · workspace"));
        assert!(snapshot.contains("j/k bounded"));
        assert!(snapshot.contains("h/l files"));
        assert!(snapshot.contains("a ask"));
        assert!(snapshot.contains("c feedback"));
        assert!(!snapshot.contains('�'));
        assert_eq!(snapshot.lines().count(), 24);
    }

    #[test]
    fn snapshots_cover_split_palette_and_growing_composer_states() {
        let split = render_snapshot(100, 28, "split").unwrap();
        assert!(split.contains("· split ·"));
        assert!(split.contains("│  11 +     let isolated_questions"));
        let narrow_split = render_snapshot(72, 22, "split").unwrap();
        assert!(narrow_split.contains("let isolated_questions ="));
        assert!(narrow_split.contains("true;"));
        assert!(!narrow_split.contains('…'));

        let command = render_snapshot(100, 28, "command").unwrap();
        assert!(command.contains("command palette"));
        assert!(command.contains("❯ diff split"));

        let files = render_snapshot(100, 28, "files").unwrap();
        assert!(files.contains("files · j/k preview"));
        assert!(files.contains("▾ demo (2 files)"));
        assert!(files.contains("src/lib.rs +1 -0"));
        assert!(files.contains("README.md +1 -0"));

        let visual = render_snapshot(100, 28, "visual").unwrap();
        assert!(visual.contains("VISUAL LINE · 3 selected"));
        assert!(visual.contains("VISUAL ·"));

        let questions = render_snapshot(120, 30, "questions").unwrap();
        assert!(questions.contains("questions · j/k · ↵ · a continue"));
        assert!(questions.contains("Why should every review"));
        assert!(questions.contains("gpt-5.2 · high"));
        assert!(questions.contains("claude-sonnet-4.5"));

        let panes = render_snapshot(140, 30, "panes").unwrap();
        assert!(panes.contains("files · j/k preview"));
        assert!(panes.contains("questions · j/k"));
        assert!(panes.contains("src/lib.rs"));
        assert!(panes.contains("Why should every review"));

        let responsive_panes = render_snapshot(107, 30, "panes").unwrap();
        assert!(responsive_panes.contains("questions · j/k"));
        assert!(responsive_panes.contains("Why should every review"));
        assert!(!responsive_panes.contains("files · j/k preview"));

        let narrow_focused_pane = render_snapshot(70, 24, "panes").unwrap();
        assert!(narrow_focused_pane.contains("questions · j/k"));
        assert!(narrow_focused_pane.contains("Why should every review"));
        assert!(!narrow_focused_pane.contains("files · j/k preview"));

        let cleared = render_snapshot(100, 28, "cleared-question").unwrap();
        assert!(cleared.contains("cleared · a/i/Enter starts fresh"));
        assert!(cleared.contains("detached from its previous Copilot session"));

        let editing = render_snapshot(100, 28, "edit-feedback").unwrap();
        assert!(editing.contains("edit feedback"));
        assert!(editing.contains("Keep isolated question sessions"));
        assert!(editing.contains("EDIT FEEDBACK"));

        let long_feedback = render_snapshot(58, 20, "long-feedback").unwrap();
        assert!(long_feedback.contains("Feedback · lines 10-11"));
        assert!(long_feedback.contains("intentionally long enough to wrap"));
        assert!(long_feedback.contains("FEEDBACK_TAIL"));

        let models = render_snapshot(100, 28, "question-models").unwrap();
        assert!(models.contains("model 1/3"));
        assert!(models.contains("GPT-5.2 · 128000 ctx · current"));
        assert!(models.contains("Claude Sonnet 4.5 · 200000 ctx"));
        assert!(models.contains("Gemini 3 Pro · 1000000 ctx"));

        let model_search = render_snapshot(100, 28, "model-search").unwrap();
        assert!(model_search.contains("claude"));
        assert!(model_search.contains("1 match(es)"));
        assert!(model_search.contains("Claude Sonnet 4.5"));
        assert!(!model_search.contains("Gemini 3 Pro"));

        let resolve = render_snapshot(100, 28, "resolve-confirm").unwrap();
        assert!(resolve.contains("confirm resolve"));
        assert!(resolve.contains("y = confirm"));
        let narrow_resolve = render_snapshot(36, 9, "resolve-confirm").unwrap();
        assert!(narrow_resolve.contains("y = confirm"));
        assert!(narrow_resolve.contains("n/Esc = cancel"));

        let resolved = render_snapshot(100, 28, "resolved-history").unwrap();
        assert!(resolved.contains("[resolved]"));
        assert!(resolved.contains("Resolved manually"));

        let stale = render_snapshot(100, 28, "stale-dismissed").unwrap();
        assert!(stale.contains("[auto-dismissed]"));
        assert!(stale.contains("changed or disappeared"));

        let narrow_model = render_snapshot(36, 9, "model-search").unwrap();
        assert!(narrow_model.contains("↵ next"));
        assert!(narrow_model.contains("Esc back"));
        assert!(narrow_model.contains("claude"));

        let composer = render_snapshot(100, 28, "composer").unwrap();
        assert!(composer.contains("rows 13-30 of 30"));
        assert!(composer.contains("↑/↓ scroll"));
        assert!(composer.contains("COPILOT"));

        let help = render_snapshot(100, 28, "help").unwrap();
        assert!(help.contains("Review navigation"));
        assert!(help.contains("Shift+↑"));
        assert!(help.contains("HELP ·"));

        let help_bottom = render_snapshot(100, 28, "help-bottom").unwrap();
        assert!(help_bottom.contains("Non-interactive CLI"));
        assert!(help_bottom.contains("rev delete PATH"));

        let complete_help = help_lines().into_iter().flat_map(|line| line.spans).fold(
            String::new(),
            |mut text, span| {
                text.push_str(span.content.as_ref());
                text
            },
        );
        assert!(complete_help.contains("Ctrl-C"));
        assert!(complete_help.contains(":diff split"));
        assert!(complete_help.contains("reveal ten lines"));
        assert!(complete_help.contains("reveal every remaining line"));
        assert!(complete_help.contains("rev delete PATH"));
    }

    #[test]
    fn help_command_opens_a_scrollable_reference_and_escape_closes_it() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let paths = AppPaths {
            data: "/tmp/rev-help/data".into(),
            cache: "/tmp/rev-help/cache".into(),
            database: "/tmp/rev-help/rev.db".into(),
            roots: "/tmp/rev-help/roots".into(),
            prs: "/tmp/rev-help/prs".into(),
            exports: "/tmp/rev-help/exports".into(),
            skills: "/tmp/rev-help/skills".into(),
            plugins: "/tmp/rev-help/plugins".into(),
        };

        execute_command(&mut state, &storage, &paths, "help").unwrap();
        assert_eq!(state.mode, RevMode::Help);
        assert_eq!(state.help_scroll, 0);

        handle_help_key(
            &mut state,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        );
        assert_eq!(state.help_scroll, 10);

        handle_help_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(state.mode, RevMode::Normal);
    }

    #[test]
    fn file_tree_opens_with_t_navigates_groups_and_selects_a_file() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let mut highlighter = PlainHighlighter;
        let paths = AppPaths {
            data: "/tmp/rev-files/data".into(),
            cache: "/tmp/rev-files/cache".into(),
            database: "/tmp/rev-files/rev.db".into(),
            roots: "/tmp/rev-files/roots".into(),
            prs: "/tmp/rev-files/prs".into(),
            exports: "/tmp/rev-files/exports".into(),
            skills: "/tmp/rev-files/skills".into(),
            plugins: "/tmp/rev-files/plugins".into(),
        };
        state.row_cursor = 2;

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::FilePicker);
        assert_eq!(state.file_picker_cursor, 1);

        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        assert_eq!(state.file_index, 1);
        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(state.mode, RevMode::Normal);
        assert_eq!(state.file_index, 1);
        assert_eq!(state.row_cursor, 0);

        state.row_cursor = 1;
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        );
        assert_eq!(state.file_index, 0);
        assert_eq!(state.row_cursor, 2);
        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        assert_eq!(state.file_index, 1);
        assert_eq!(state.row_cursor, 1);
        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        assert_eq!(state.mode, RevMode::Normal);
        assert_eq!(state.row_cursor, 1);

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        );
        assert!(state.collapsed_repos.contains(&0));
        assert_eq!(state.file_picker_cursor, 0);
        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
        );
        assert!(!state.collapsed_repos.contains(&0));
    }

    #[test]
    fn visual_mode_is_explicit_and_survives_a_file_tree_round_trip() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let mut highlighter = PlainHighlighter;
        let paths = AppPaths {
            data: "/tmp/rev-visual/data".into(),
            cache: "/tmp/rev-visual/cache".into(),
            database: "/tmp/rev-visual/rev.db".into(),
            roots: "/tmp/rev-visual/roots".into(),
            prs: "/tmp/rev-visual/prs".into(),
            exports: "/tmp/rev-visual/exports".into(),
            skills: "/tmp/rev-visual/skills".into(),
            plugins: "/tmp/rev-visual/plugins".into(),
        };

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Visual);
        assert_eq!(state.visual_anchor, Some(0));

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::FilePicker);

        handle_file_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        assert_eq!(state.mode, RevMode::Visual);
        assert_eq!(state.visual_anchor, Some(0));
        assert!(state.status.contains("Selection retained"));
    }

    #[test]
    fn q_toggles_questions_without_quitting_and_command_q_is_the_only_exit() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-q-only-questions");

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Questions);
        assert!(state.questions_open);
        assert_ne!(state.status, "quit");

        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Normal);
        assert!(!state.questions_open);

        state.mode = RevMode::History;
        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Questions);
        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::History);

        state.queued_questions.push_back(QuestionLaunch {
            annotation_id: "queued-question".into(),
            pending: None,
            existing: None,
        });
        state.mode = RevMode::Command;
        state.command.input = "q".into();
        state.command.cursor = 1;
        state.command.selected = 0;
        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert_ne!(state.status, "quit");
        assert!(state.status.contains("active or queued"));

        state.queued_questions.clear();
        state.mode = RevMode::Command;
        state.command.input = "q".into();
        state.command.cursor = 1;
        state.command.selected = 0;
        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.status, "quit");
    }

    #[test]
    fn left_and_right_panes_coexist_and_keep_independent_focus() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-panes");

        open_file_picker(&mut state);
        assert!(state.file_tree_open);
        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(state.file_tree_open);
        assert!(state.questions_open);
        assert_eq!(state.mode, RevMode::Questions);

        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(state.file_tree_open);
        assert!(!state.questions_open);
        assert_eq!(state.mode, RevMode::FilePicker);

        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        state.question_cursor = 1;
        handle_question_key(
            &mut state,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(state.file_index, 1);
        assert_eq!(state.file_picker_cursor, 2);
    }

    #[test]
    fn visual_text_is_copy_only_and_visual_selection_survives_nested_panes() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-visual-pane-order");

        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| row.yank_text.starts_with("Question ·"))
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(state.visual_row_anchor.is_some());
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Visual);
        assert!(state.compose_target.is_none());
        assert!(state.status.contains("copy-only"));

        state.visual_row_anchor = None;
        state.visual_anchor = Some(0);
        state.row_cursor = 0;
        open_file_picker(&mut state);
        open_questions(&mut state);
        close_file_picker(&mut state);
        close_questions(&mut state);
        assert_eq!(state.mode, RevMode::Visual);
        assert_eq!(state.visual_anchor, Some(0));
    }

    #[test]
    fn visual_chat_and_code_yanks_and_yy_copy_complete_items() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-yank");

        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| row.item_text.contains("It keeps model choice"))
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(state.visual_row_anchor.is_some());
        assert_eq!(
            current_item_yank_text(&mut state, &mut highlighter),
            "It keeps model choice, history, and follow-ups scoped to one thread."
        );
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        let selected = visual_selection_yank_text(&mut state, &mut highlighter);
        assert!(selected.contains("It keeps model choice"));
        assert!(selected.lines().count() >= 1);

        state.mode = RevMode::Normal;
        state.visual_row_anchor = None;
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(state.pending_yank);
        assert!(state.status.contains("press y again"));

        state.pending_yank = false;
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| row.yank_text.starts_with("Question ·"))
            .unwrap();
        assert_eq!(
            current_item_yank_text(&mut state, &mut highlighter),
            "Why should every review question use an isolated session?"
        );
    }

    #[test]
    fn contextual_dd_deletes_comments_but_never_question_threads() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_feedback(&mut state);
        let feedback = state.annotations.last().cloned().unwrap();
        storage.add_annotation(&feedback.0, &feedback.1).unwrap();
        seed_snapshot_questions(&mut state);
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-delete-comment");

        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| row.item_text.contains("Keep isolated question sessions"))
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        )
        .unwrap();
        for _ in 0..2 {
            handle_key(
                &mut state,
                &storage,
                &paths,
                &mut highlighter,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            )
            .unwrap();
        }
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(storage
            .annotation_by_id("feedback-architecture")
            .unwrap()
            .is_some());
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(storage
            .annotation_by_id("feedback-architecture")
            .unwrap()
            .is_none());
        assert!(state
            .annotations
            .iter()
            .all(|(annotation, _)| annotation.id != "feedback-architecture"));
        assert_eq!(
            state
                .annotations
                .iter()
                .filter(|(annotation, _)| annotation.kind == AnnotationKind::Ask)
                .count(),
            2
        );

        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| row.yank_text.starts_with("Question ·"))
            .unwrap();
        for _ in 0..2 {
            handle_review_key(
                &mut state,
                &storage,
                &paths,
                &mut highlighter,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(
            state
                .annotations
                .iter()
                .filter(|(annotation, _)| annotation.kind == AnnotationKind::Ask)
                .count(),
            2
        );
        assert!(state.status.contains("saved feedback only"));

        state.mode = RevMode::History;
        state.history_cursor = 0;
        for _ in 0..2 {
            handle_history_key(
                &mut state,
                &storage,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(
            state
                .annotations
                .iter()
                .filter(|(annotation, _)| annotation.kind == AnnotationKind::Ask)
                .count(),
            2
        );
        assert!(state.status.contains("saved feedback only"));
    }

    #[test]
    fn question_panel_navigates_to_a_thread_and_starts_a_follow_up() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let mut highlighter = PlainHighlighter;
        let paths = AppPaths {
            data: "/tmp/rev-questions/data".into(),
            cache: "/tmp/rev-questions/cache".into(),
            database: "/tmp/rev-questions/rev.db".into(),
            roots: "/tmp/rev-questions/roots".into(),
            prs: "/tmp/rev-questions/prs".into(),
            exports: "/tmp/rev-questions/exports".into(),
            skills: "/tmp/rev-questions/skills".into(),
            plugins: "/tmp/rev-questions/plugins".into(),
        };

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Questions);

        handle_question_key(
            &mut state,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE),
        );
        assert_eq!(state.mode, RevMode::Normal);
        execute_command(&mut state, &storage, &paths, "questions").unwrap();
        assert_eq!(state.mode, RevMode::Questions);

        handle_question_key(
            &mut state,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        handle_question_key(
            &mut state,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        );

        assert_eq!(state.file_index, 1);
        assert_eq!(state.mode, RevMode::Compose);
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::FollowUp(ref id)) if id == "question-docs"
        ));
        assert!(state.status.contains("FOLLOW-UP"));
    }

    #[test]
    fn a_resumes_the_question_on_the_exact_source_anchor_and_asks_elsewhere() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-contextual-a");

        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    super::RevRowKind::Source {
                        visible_index: 1,
                        ..
                    }
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::FollowUp(ref id)) if id == "question-architecture"
        ));

        state.mode = RevMode::Normal;
        state.compose_target = None;
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    super::RevRowKind::Source {
                        visible_index: 0,
                        ..
                    }
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::NewQuestion)
        ));
    }

    #[test]
    fn contextual_actions_refuse_duplicate_anchors_and_wrong_inline_kinds() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let (mut duplicate, mut duplicate_placement) =
            state.annotation("question-architecture").unwrap().clone();
        duplicate.id = "question-architecture-duplicate".into();
        duplicate_placement.annotation_id = duplicate.id.clone();
        state
            .annotations
            .push((duplicate.clone(), duplicate_placement.clone()));
        state.threads.insert(duplicate.id.clone(), Vec::new());
        state.question_sessions.insert(
            duplicate.id.clone(),
            RevQuestionSession {
                annotation_id: duplicate.id.clone(),
                session_id: "duplicate-session".into(),
                model_id: "gpt-5".into(),
                reasoning_effort: None,
                context_tier: None,
                state: "ready".into(),
                created_at: crate::storage::now(),
                updated_at: crate::storage::now(),
            },
        );
        let comment = crate::domain::Annotation {
            id: "comment-on-question-anchor".into(),
            kind: crate::domain::AnnotationKind::Comment,
            text: Some("Same anchor, different kind".into()),
            ..duplicate
        };
        let comment_placement = crate::domain::Placement {
            annotation_id: comment.id.clone(),
            ..duplicate_placement
        };
        state.annotations.push((comment, comment_placement));
        state.invalidate_rows();
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-ambiguous-anchor");
        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    super::RevRowKind::Source {
                        visible_index: 1,
                        ..
                    }
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Normal);
        assert!(state.compose_target.is_none());
        assert!(state.status.contains("Multiple saved ask items"));

        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    &row.kind,
                    super::RevRowKind::Annotation { annotation_id, .. }
                        if annotation_id == "comment-on-question-anchor"
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Normal);
        assert!(state.compose_target.is_none());
        assert!(state.status.contains("This row is comment"));
    }

    #[test]
    fn old_side_multiline_annotation_ending_on_context_is_rendered() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        state.workspace.repos[0].diff = parse_unified(concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -10,3 +10,2 @@\n",
            " context ten\n",
            "-removed eleven\n",
            " context twelve\n",
        ))
        .unwrap();
        let repo = &state.workspace.repos[0];
        state.annotations.push((
            crate::domain::Annotation {
                id: "old-side-feedback".into(),
                repo_id: repo.record.id.clone(),
                kind: crate::domain::AnnotationKind::Comment,
                file_path: "src/lib.rs".into(),
                anchor_snippet: "removed eleven\ncontext twelve".into(),
                anchor_hash: "old-side".into(),
                anchor_start_offset: 0,
                anchor_line_count: 2,
                text: Some("Review the removed flow".into()),
                submitted: false,
                delivery_state: crate::domain::DeliveryState::Draft,
                status: crate::domain::AnnotationStatus::Active,
                status_reason: None,
                status_changed_at: None,
                created_at: crate::storage::now(),
            },
            crate::domain::Placement {
                annotation_id: "old-side-feedback".into(),
                version_id: repo.version.id.clone(),
                side: crate::domain::AnchorSide::Old,
                line_start: 11,
                line_end: 12,
                outdated: false,
                ambiguous: false,
            },
        ));
        state.invalidate_rows();
        let mut highlighter = PlainHighlighter;
        row_count(&mut state, &mut highlighter);

        assert!(state.render_cache.as_ref().unwrap().rows.iter().any(|row| {
            matches!(
                &row.kind,
                super::RevRowKind::Annotation { annotation_id, .. }
                    if annotation_id == "old-side-feedback"
            )
        }));
    }

    #[test]
    fn c_edits_only_feedback_with_the_exact_original_multiline_selection() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let repo = &state.workspace.repos[0];
        let annotation = crate::domain::Annotation {
            id: "feedback-lines-10-11".into(),
            repo_id: repo.record.id.clone(),
            kind: crate::domain::AnnotationKind::Comment,
            file_path: "src/lib.rs".into(),
            anchor_snippet: "fn review() {\n    let isolated_questions = true;".into(),
            anchor_hash: "comment-anchor".into(),
            anchor_start_offset: 0,
            anchor_line_count: 2,
            text: Some("Original feedback".into()),
            submitted: false,
            delivery_state: crate::domain::DeliveryState::Draft,
            status: crate::domain::AnnotationStatus::Active,
            status_reason: None,
            status_changed_at: None,
            created_at: crate::storage::now(),
        };
        let placement = crate::domain::Placement {
            annotation_id: annotation.id.clone(),
            version_id: repo.version.id.clone(),
            side: crate::domain::AnchorSide::New,
            line_start: 10,
            line_end: 11,
            outdated: false,
            ambiguous: false,
        };
        storage.add_annotation(&annotation, &placement).unwrap();
        state.annotations.push((annotation, placement));
        state.invalidate_rows();
        let mut highlighter = PlainHighlighter;
        let paths = test_paths("rev-contextual-c");
        row_count(&mut state, &mut highlighter);
        state.visual_anchor = Some(0);
        state.mode = RevMode::Visual;
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    super::RevRowKind::Source {
                        visible_index: 1,
                        ..
                    }
                )
            })
            .unwrap();

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::EditFeedback(ref id)) if id == "feedback-lines-10-11"
        ));
        assert_eq!(state.compose, "Original feedback");

        state.compose = "Updated feedback".into();
        state.compose_cursor = state.compose.len();
        handle_compose_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(
            storage
                .annotation_by_id("feedback-lines-10-11")
                .unwrap()
                .unwrap()
                .text
                .as_deref(),
            Some("Updated feedback")
        );

        row_count(&mut state, &mut highlighter);
        state.visual_anchor = Some(0);
        state.mode = RevMode::Visual;
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    super::RevRowKind::Source {
                        visible_index: 1,
                        ..
                    }
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        )
        .unwrap();
        storage.delete_annotation("feedback-lines-10-11").unwrap();
        state.compose = "Draft that must survive a failed save".into();
        state.compose_cursor = state.compose.len();
        handle_compose_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Compose);
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::EditFeedback(ref id)) if id == "feedback-lines-10-11"
        ));
        assert_eq!(state.compose, "Draft that must survive a failed save");
        assert_eq!(state.visual_anchor, Some(0));
        assert!(state.status.contains("draft and selection retained"));

        state.compose.clear();
        state.compose_target = None;
        row_count(&mut state, &mut highlighter);
        state.visual_anchor = Some(2);
        state.mode = RevMode::Visual;
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    &row.kind,
                    super::RevRowKind::Annotation { annotation_id, .. }
                        if annotation_id == "feedback-lines-10-11"
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::Feedback)
        ));
        assert!(state.compose.is_empty());

        state.compose_target = None;
        state.compose.clear();
        state.mode = RevMode::Normal;
        state.visual_anchor = None;
        state
            .annotations
            .iter_mut()
            .find(|(annotation, _)| annotation.id == "feedback-lines-10-11")
            .unwrap()
            .1
            .outdated = true;
        state.invalidate_rows();
        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    &row.kind,
                    super::RevRowKind::Annotation { annotation_id, .. }
                        if annotation_id == "feedback-lines-10-11"
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::Normal);
        assert!(state.compose_target.is_none());
        assert!(state.status.contains("outdated or ambiguous"));
    }

    #[test]
    fn slash_clear_resets_only_one_question_and_next_a_starts_fresh_context() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let (annotation, placement) = state.annotation("question-architecture").unwrap().clone();
        storage.add_annotation(&annotation, &placement).unwrap();
        for message in state.threads["question-architecture"].clone() {
            storage.append_ask_message(&message).unwrap();
        }
        let session = state.question_sessions["question-architecture"].clone();
        storage
            .upsert_rev_question_session(
                &session,
                state.threads["question-architecture"]
                    .first()
                    .map(|message| message.id.as_str()),
                None,
            )
            .unwrap();
        storage
            .activate_session(&crate::domain::SessionRecord {
                id: session.session_id,
                work_item_id: state.workspace.item.id.clone(),
                parent_id: None,
                active: true,
                created_at: crate::storage::now(),
            })
            .unwrap();
        state.agent = Some(RevAgentSlot {
            question_id: "different-background-question".into(),
            runtime: Box::new(NoopAgent),
            pending: None,
            outbound_id: None,
            session_id: Some("different-session".into()),
            selection: ModelSelection {
                model_id: "test".into(),
                reasoning_effort: None,
                context_tier: None,
            },
            needs_model_picker: false,
            model_switch_only: false,
        });
        state.agent_activity = "Answering the unrelated question".into();
        state.mode = RevMode::Compose;
        state.compose_target = Some(ComposeTarget::FollowUp("question-architecture".into()));
        state.compose = "/clear".into();
        state.compose_cursor = state.compose.len();
        let paths = test_paths("rev-thread-clear");

        handle_compose_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert!(state.threads["question-architecture"].is_empty());
        assert!(!state
            .question_sessions
            .contains_key("question-architecture"));
        assert!(storage
            .ask_messages_for_annotation("question-architecture")
            .unwrap()
            .is_empty());
        assert!(storage
            .rev_question_session("question-architecture")
            .unwrap()
            .is_none());
        assert!(storage
            .active_session(&state.workspace.item.id)
            .unwrap()
            .is_none());
        assert_eq!(
            state.agent_activity, "Answering the unrelated question",
            "clearing an idle thread must not hide another thread's progress"
        );

        let mut highlighter = PlainHighlighter;
        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    super::RevRowKind::Source {
                        visible_index: 1,
                        ..
                    }
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::FollowUp(ref id)) if id == "question-architecture"
        ));
        state.compose = "Start over from this anchor".into();
        state.compose_cursor = state.compose.len();
        handle_compose_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        let launch = state.queued_questions.back().unwrap();
        assert!(launch.existing.is_none());
        assert!(launch.pending.as_ref().unwrap().needs_model_picker);
        assert!(launch
            .pending
            .as_ref()
            .unwrap()
            .prompt
            .contains("Selected code:"));
    }

    #[test]
    fn model_switch_updates_the_existing_question_session_without_sending_a_prompt() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let (annotation, placement) = state.annotations[0].clone();
        storage.add_annotation(&annotation, &placement).unwrap();
        let original = state
            .question_sessions
            .get("question-architecture")
            .cloned()
            .unwrap();
        for message in state.threads["question-architecture"].clone() {
            storage.append_ask_message(&message).unwrap();
        }
        storage
            .upsert_rev_question_session(
                &original,
                state.threads["question-architecture"]
                    .first()
                    .map(|message| message.id.as_str()),
                None,
            )
            .unwrap();
        state.mode = RevMode::Model;
        state.model_return_mode = RevMode::Questions;
        state.agent = Some(RevAgentSlot {
            question_id: "question-architecture".into(),
            runtime: Box::new(NoopAgent),
            pending: None,
            outbound_id: None,
            session_id: Some(original.session_id.clone()),
            selection: ModelSelection {
                model_id: original.model_id,
                reasoning_effort: original.reasoning_effort,
                context_tier: original.context_tier,
            },
            needs_model_picker: false,
            model_switch_only: true,
        });
        let paths = AppPaths {
            data: "/tmp/rev-model-switch/data".into(),
            cache: "/tmp/rev-model-switch/cache".into(),
            database: "/tmp/rev-model-switch/rev.db".into(),
            roots: "/tmp/rev-model-switch/roots".into(),
            prs: "/tmp/rev-model-switch/prs".into(),
            exports: "/tmp/rev-model-switch/exports".into(),
            skills: "/tmp/rev-model-switch/skills".into(),
            plugins: "/tmp/rev-model-switch/plugins".into(),
        };

        handle_agent_event(
            &mut state,
            &storage,
            &paths,
            AgentEvent::ModelSelectionChanged(ModelSelection {
                model_id: "gpt-5.3-codex".into(),
                reasoning_effort: Some("high".into()),
                context_tier: Some("large".into()),
            }),
        )
        .unwrap();

        assert_eq!(state.mode, RevMode::Questions);
        assert!(state.agent.is_none());
        assert_eq!(
            state
                .question_sessions
                .get("question-architecture")
                .map(|session| session.model_id.as_str()),
            Some("gpt-5.3-codex")
        );
        assert_eq!(
            storage
                .rev_question_session("question-architecture")
                .unwrap()
                .map(|session| session.model_id),
            Some("gpt-5.3-codex".into())
        );
        assert!(state.status.contains("future follow-ups"));
    }

    #[test]
    fn model_picker_starts_on_the_threads_current_runtime_model() {
        let picker = ModelPicker::new(
            vec![
                ModelOption {
                    id: "fast".into(),
                    name: "Fast".into(),
                    supported_reasoning_efforts: vec![],
                    default_reasoning_effort: None,
                    max_context_tokens: None,
                    context_tiers: vec![],
                },
                ModelOption {
                    id: "deep".into(),
                    name: "Deep".into(),
                    supported_reasoning_efforts: vec!["high".into()],
                    default_reasoning_effort: Some("high".into()),
                    max_context_tokens: Some(128_000),
                    context_tiers: vec![],
                },
            ],
            ModelSelection {
                model_id: "deep".into(),
                reasoning_effort: Some("high".into()),
                context_tier: None,
            },
            PickerScope::Thread,
        );

        assert_eq!(picker.index, 1);
        assert_eq!(picker.model_index, 1);
        assert_eq!(picker.model().id, "deep");
    }

    #[test]
    fn model_picker_searches_ids_and_names_without_losing_stable_indices() {
        let mut picker = ModelPicker::new(
            vec![
                ModelOption {
                    id: "gpt-fast".into(),
                    name: "GPT Fast".into(),
                    supported_reasoning_efforts: vec![],
                    default_reasoning_effort: None,
                    max_context_tokens: None,
                    context_tiers: vec![],
                },
                ModelOption {
                    id: "claude-deep".into(),
                    name: "Claude Sonnet".into(),
                    supported_reasoning_efforts: vec![],
                    default_reasoning_effort: None,
                    max_context_tokens: None,
                    context_tiers: vec![],
                },
            ],
            ModelSelection {
                model_id: "gpt-fast".into(),
                reasoning_effort: None,
                context_tier: None,
            },
            PickerScope::GlobalDefault,
        );
        picker.query = "SONNET".into();

        assert_eq!(picker.filtered_model_indices(), vec![1]);
        picker.query = "claude-deep".into();
        assert_eq!(picker.filtered_model_indices(), vec![1]);
        picker.query = "missing".into();
        assert!(picker.filtered_model_indices().is_empty());
    }

    #[test]
    fn model_picker_escape_goes_back_then_uses_startup_model_for_new_question() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let models = vec![ModelOption {
            id: "startup".into(),
            name: "Startup".into(),
            supported_reasoning_efforts: vec!["high".into()],
            default_reasoning_effort: Some("high".into()),
            max_context_tokens: None,
            context_tiers: vec![],
        }];
        let selection = ModelSelection {
            model_id: "startup".into(),
            reasoning_effort: None,
            context_tier: None,
        };
        let mut picker = ModelPicker::new(models, selection.clone(), PickerScope::Thread);
        picker.stage = PickerStage::Context;
        state.picker = Some(picker);
        state.mode = RevMode::Model;
        state.agent = Some(RevAgentSlot {
            question_id: "new-question".into(),
            runtime: Box::new(NoopAgent),
            pending: Some(PendingSend {
                annotation_id: "new-question".into(),
                user_message_id: "user".into(),
                assistant_message_id: "assistant".into(),
                assistant_seq: 1,
                prompt: "original question".into(),
                needs_model_picker: true,
            }),
            outbound_id: None,
            session_id: Some("session".into()),
            selection,
            needs_model_picker: true,
            model_switch_only: false,
        });
        let paths = test_paths("rev-model-cancel");

        super::handle_model_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.picker.as_ref().unwrap().stage, PickerStage::Reasoning);
        super::handle_model_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.picker.as_ref().unwrap().stage, PickerStage::Model);
        super::handle_model_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(state.mode, RevMode::Normal);
        assert!(state.streaming);
        assert!(state.picker.is_none());
        let agent = state.agent.as_ref().unwrap();
        assert!(agent.outbound_id.is_some());
        assert!(agent.pending.is_some());
        assert!(!agent.needs_model_picker);
        assert!(state.status.contains("startup model"));
    }

    #[test]
    fn global_model_picker_persists_only_after_the_context_stage() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        state.mode = RevMode::Model;
        let mut picker = ModelPicker::new(
            vec![ModelOption {
                id: "deep".into(),
                name: "Deep".into(),
                supported_reasoning_efforts: vec!["high".into()],
                default_reasoning_effort: Some("high".into()),
                max_context_tokens: Some(128_000),
                context_tiers: vec![],
            }],
            ModelSelection {
                model_id: "deep".into(),
                reasoning_effort: Some("high".into()),
                context_tier: None,
            },
            PickerScope::GlobalDefault,
        );
        picker.stage = PickerStage::Context;
        state.picker = Some(picker);
        let paths = test_paths("rev-global-model");

        super::handle_model_key(
            &mut state,
            &storage,
            &paths,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(state.mode, RevMode::Normal);
        assert_eq!(state.default_model.as_ref().unwrap().model_id, "deep");
        let persisted: ModelSelection = serde_json::from_str(
            &storage
                .setting(super::REV_DEFAULT_MODEL_SETTING)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.model_id, "deep");
    }

    #[test]
    fn resolving_inline_feedback_requires_confirmation_and_keeps_history() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_feedback(&mut state);
        let (annotation, placement) = state.annotation("feedback-architecture").unwrap().clone();
        storage.add_annotation(&annotation, &placement).unwrap();
        let mut highlighter = PlainHighlighter;
        row_count(&mut state, &mut highlighter);
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    &row.kind,
                    super::RevRowKind::Annotation { annotation_id, .. }
                        if annotation_id == "feedback-architecture"
                )
            })
            .unwrap();
        let paths = test_paths("rev-resolve-confirmation");

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.mode, RevMode::ConfirmResolve);
        assert_eq!(
            storage
                .annotation_by_id("feedback-architecture")
                .unwrap()
                .unwrap()
                .status,
            AnnotationStatus::Active
        );

        handle_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(
            storage
                .annotation_by_id("feedback-architecture")
                .unwrap()
                .unwrap()
                .status,
            AnnotationStatus::Resolved
        );
        assert!(state
            .annotations
            .iter()
            .any(|(annotation, _)| annotation.id == "feedback-architecture"));
        assert!(!build_rows(&state, 80, &mut highlighter).iter().any(|row| {
            matches!(
                &row.kind,
                super::RevRowKind::Annotation { annotation_id, .. }
                    if annotation_id == "feedback-architecture"
            )
        }));
    }

    #[test]
    fn inactive_question_leaves_panel_and_reopen_restores_it() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_questions(&mut state);
        let (annotation, placement) = state.annotations[0].clone();
        storage.add_annotation(&annotation, &placement).unwrap();
        let changed_at = storage
            .set_annotation_status(
                &annotation.id,
                AnnotationStatus::AutoDismissed,
                Some("selected code changed"),
            )
            .unwrap();
        state.annotations[0].0.status = AnnotationStatus::AutoDismissed;
        state.annotations[0].0.status_reason = Some("selected code changed".into());
        state.annotations[0].0.status_changed_at = Some(changed_at);

        assert!(!question_ids(&state).contains(&annotation.id));
        state.mode = RevMode::History;
        state.history_cursor = 0;
        handle_history_key(
            &mut state,
            &storage,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(question_ids(&state).contains(&annotation.id));
        assert_eq!(state.annotations[0].0.status, AnnotationStatus::Active);
    }

    #[test]
    fn completed_background_answer_does_not_close_an_unrelated_composer() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        state.mode = RevMode::Compose;
        state.compose_target = Some(ComposeTarget::NewQuestion);
        state.compose = "keep this draft".into();
        state.agent = Some(RevAgentSlot {
            question_id: "background".into(),
            runtime: Box::new(NoopAgent),
            pending: None,
            outbound_id: None,
            session_id: Some("session".into()),
            selection: ModelSelection {
                model_id: "test".into(),
                reasoning_effort: None,
                context_tier: None,
            },
            needs_model_picker: false,
            model_switch_only: false,
        });
        let paths = AppPaths {
            data: "/tmp/rev-background/data".into(),
            cache: "/tmp/rev-background/cache".into(),
            database: "/tmp/rev-background/rev.db".into(),
            roots: "/tmp/rev-background/roots".into(),
            prs: "/tmp/rev-background/prs".into(),
            exports: "/tmp/rev-background/exports".into(),
            skills: "/tmp/rev-background/skills".into(),
            plugins: "/tmp/rev-background/plugins".into(),
        };

        finish_active_question(&mut state, &paths);

        assert_eq!(state.mode, RevMode::Compose);
        assert_eq!(state.compose, "keep this draft");
        assert!(matches!(
            state.compose_target,
            Some(ComposeTarget::NewQuestion)
        ));
    }

    #[test]
    fn revealed_context_has_a_distinct_muted_background() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let hunk = &mut state.workspace.repos[0].diff.files[0].hunks[0];
        hunk.lines.insert(
            0,
            DiffLine {
                kind: LineKind::Context,
                old_line: Some(9),
                new_line: Some(9),
                content: "revealed".into(),
            },
        );
        hunk.old_start = 9;
        hunk.new_start = 9;
        hunk.old_count += 1;
        hunk.new_count += 1;
        state.row_cursor = 1;
        let mut highlighter = PlainHighlighter;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();

        assert_eq!(
            terminal.backend().buffer()[(1, 3)].bg,
            Color::Rgb(38, 38, 38)
        );
    }

    #[test]
    fn hide_commands_filter_inline_items_without_deleting_history() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        seed_snapshot_feedback(&mut state);
        seed_snapshot_questions(&mut state);
        let total = state.annotations.len();
        let paths = test_paths("rev-hide-inline");
        let mut highlighter = PlainHighlighter;

        execute_command(&mut state, &storage, &paths, "hide comments").unwrap();
        let rows = build_rows(&state, 80, &mut highlighter);
        assert!(!rows.iter().any(|row| {
            matches!(
                &row.kind,
                RevRowKind::Annotation { annotation_id, .. }
                    if annotation_id == "feedback-architecture"
            )
        }));
        assert!(rows.iter().any(|row| {
            matches!(
                &row.kind,
                RevRowKind::Annotation { annotation_id, .. }
                    if annotation_id == "question-architecture"
            )
        }));

        execute_command(&mut state, &storage, &paths, "hide questions").unwrap();
        assert!(!build_rows(&state, 80, &mut highlighter)
            .iter()
            .any(|row| matches!(row.kind, RevRowKind::Annotation { .. })));
        assert_eq!(state.annotations.len(), total);
        assert_eq!(question_ids(&state).len(), 2);

        execute_command(&mut state, &storage, &paths, "show comments").unwrap();
        execute_command(&mut state, &storage, &paths, "show questions").unwrap();
        assert!(build_rows(&state, 80, &mut highlighter)
            .iter()
            .any(|row| matches!(row.kind, RevRowKind::Annotation { .. })));
    }

    #[test]
    fn review_viewport_moves_only_after_the_cursor_crosses_an_edge() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let mut highlighter = PlainHighlighter;
        let mut terminal = Terminal::new(TestBackend::new(80, 9)).unwrap();

        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        assert_eq!(state.review_scroll, 0);
        let viewport = state.review_viewport_height;
        for _ in 1..viewport {
            move_row(&mut state, &mut highlighter, 1);
            terminal
                .draw(|frame| render(frame, &mut state, &mut highlighter))
                .unwrap();
            assert_eq!(state.review_scroll, 0);
        }
        move_row(&mut state, &mut highlighter, 1);
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        assert_eq!(state.review_scroll, 1);
        move_row(&mut state, &mut highlighter, -1);
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        assert_eq!(state.review_scroll, 1);
    }

    #[test]
    fn retract_restores_original_diff_and_never_removes_changed_lines() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let hunk = &mut state.workspace.repos[0].diff.files[0].hunks[0];
        hunk.lines.insert(
            0,
            DiffLine {
                kind: LineKind::Context,
                old_line: Some(9),
                new_line: Some(9),
                content: "expanded".into(),
            },
        );
        hunk.old_start = 9;
        hunk.new_start = 9;
        hunk.old_count += 1;
        hunk.new_count += 1;
        let removed = HashSet::from([(0, Some(9), Some(9))]);

        rebuild_file_without_expanded_lines(&mut state, 0, 0, &removed).unwrap();
        let file = &state.workspace.repos[0].diff.files[0];
        assert!(!file.visible_lines().any(|line| line.content == "expanded"));
        assert!(file
            .visible_lines()
            .any(|line| line.kind == LineKind::Addition));
        assert_eq!(
            file,
            state
                .original_files
                .get(&review_file_key("rev-snapshot-repo", file.path()))
                .unwrap()
        );
    }

    #[test]
    fn overlapping_hunk_expansions_become_one_continuous_region() {
        let context = |line| DiffLine {
            kind: LineKind::Context,
            old_line: Some(line),
            new_line: Some(line),
            content: format!("line {line}"),
        };
        let mut hunks = vec![
            Hunk {
                header: "@@ -10,5 +10,5 @@".into(),
                old_start: 10,
                old_count: 5,
                new_start: 10,
                new_count: 5,
                lines: (10..15).map(context).collect(),
            },
            Hunk {
                header: "@@ -15,5 +15,5 @@".into(),
                old_start: 15,
                old_count: 5,
                new_start: 15,
                new_count: 5,
                lines: (15..20).map(context).collect(),
            },
        ];

        merge_touching_hunks(&mut hunks);

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines.len(), 10);
        assert_eq!(hunks[0].new_count, 10);
    }

    #[test]
    fn only_unchanged_line_separators_are_expandable_folds() {
        assert!(is_expandable_fold(&DiffLine {
            kind: LineKind::Meta,
            old_line: None,
            new_line: None,
            content: "··· 90 unchanged lines ··· (o expand 10 · O expand all)".into(),
        }));
        assert!(!is_expandable_fold(&DiffLine {
            kind: LineKind::Meta,
            old_line: None,
            new_line: None,
            content: "\\ No newline at end of file".into(),
        }));
    }

    #[test]
    fn o_and_shift_o_expand_the_fold_under_the_cursor() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let test_root = std::env::temp_dir().join(format!(
            "rev-fold-expand-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(test_root.join("src")).unwrap();
        std::fs::write(
            test_root.join("src/lib.rs"),
            (1..=30)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        state.workspace.repos[0].record.path = test_root.clone();
        state.workspace.repos[0].version.worktree_path = Some(test_root.clone());
        state.workspace.repos[0].diff = parse_unified(
            "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2 +2 @@
-old 2
+line 2
@@ -25 +25 @@
-old 25
+line 25
",
        )
        .unwrap();
        state.invalidate_rows();

        let paths = AppPaths {
            data: test_root.join("data"),
            cache: test_root.join("cache"),
            database: test_root.join("rev.db"),
            roots: test_root.join("roots"),
            prs: test_root.join("prs"),
            exports: test_root.join("exports"),
            skills: test_root.join("skills"),
            plugins: test_root.join("plugins"),
        };
        let mut highlighter = PlainHighlighter;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    &row.kind,
                    super::RevRowKind::Source { line, .. } if line.kind == LineKind::Meta
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.workspace.repos[0].diff.files[0].hunks.len(), 2);
        assert_eq!(
            state.workspace.repos[0].diff.files[0].hunks[0]
                .lines
                .iter()
                .filter(|line| line.kind == LineKind::Context)
                .count(),
            10
        );

        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        state.row_cursor = state
            .render_cache
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|row| {
                matches!(
                    &row.kind,
                    super::RevRowKind::Source { line, .. } if line.kind == LineKind::Meta
                )
            })
            .unwrap();
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
        )
        .unwrap();
        assert_eq!(state.workspace.repos[0].diff.files[0].hunks.len(), 1);
        assert!(!state.workspace.repos[0].diff.files[0].hunks[0]
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Meta));

        std::fs::remove_dir_all(test_root).unwrap();
    }

    #[test]
    fn a_question_starting_up_is_not_replaced_by_the_next_question() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        state.agent = Some(RevAgentSlot {
            question_id: "first".into(),
            runtime: Box::new(NoopAgent),
            pending: None,
            outbound_id: None,
            session_id: None,
            selection: ModelSelection {
                model_id: "test".into(),
                reasoning_effort: None,
                context_tier: None,
            },
            needs_model_picker: true,
            model_switch_only: false,
        });
        let paths = AppPaths {
            data: "/tmp/rev-queue/data".into(),
            cache: "/tmp/rev-queue/cache".into(),
            database: "/tmp/rev-queue/rev.db".into(),
            roots: "/tmp/rev-queue/roots".into(),
            prs: "/tmp/rev-queue/prs".into(),
            exports: "/tmp/rev-queue/exports".into(),
            skills: "/tmp/rev-queue/skills".into(),
            plugins: "/tmp/rev-queue/plugins".into(),
        };
        queue_question_launch(
            &mut state,
            &paths,
            QuestionLaunch {
                annotation_id: "second".into(),
                pending: Some(PendingSend {
                    annotation_id: "second".into(),
                    user_message_id: "user".into(),
                    assistant_message_id: "assistant".into(),
                    assistant_seq: 1,
                    prompt: "question".into(),
                    needs_model_picker: true,
                }),
                existing: None,
            },
        );

        assert_eq!(
            state.agent.as_ref().map(|agent| agent.question_id.as_str()),
            Some("first")
        );
        assert_eq!(state.queued_questions.len(), 1);
        assert!(state.status.contains("1 waiting"));
    }

    #[test]
    fn empty_ask_and_feedback_escape_close_then_preserve_selection_until_second_escape() {
        for target in [ComposeTarget::NewQuestion, ComposeTarget::Feedback] {
            let storage = Storage::in_memory().unwrap();
            let workspace = snapshot_workspace(&storage).unwrap();
            let mut state = RevState::load(workspace, &storage).unwrap();
            let mut highlighter = PlainHighlighter;
            let paths = AppPaths {
                data: "/tmp/rev-escape/data".into(),
                cache: "/tmp/rev-escape/cache".into(),
                database: "/tmp/rev-escape/rev.db".into(),
                roots: "/tmp/rev-escape/roots".into(),
                prs: "/tmp/rev-escape/prs".into(),
                exports: "/tmp/rev-escape/exports".into(),
                skills: "/tmp/rev-escape/skills".into(),
                plugins: "/tmp/rev-escape/plugins".into(),
            };
            state.visual_anchor = Some(0);
            state.mode = RevMode::Compose;
            state.compose_target = Some(target);

            handle_compose_key(
                &mut state,
                &storage,
                &paths,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            )
            .unwrap();

            assert_eq!(state.mode, RevMode::Visual);
            assert_eq!(state.visual_anchor, Some(0));
            assert!(state.compose_target.is_none());
            assert!(state.status.contains("selection retained"));

            handle_review_key(
                &mut state,
                &storage,
                &paths,
                &mut highlighter,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            )
            .unwrap();

            assert_eq!(state.mode, RevMode::Normal);
            assert_eq!(state.visual_anchor, None);
        }
    }

    #[test]
    fn vertical_navigation_cannot_cross_a_file_boundary() {
        let storage = Storage::in_memory().unwrap();
        let workspace = snapshot_workspace(&storage).unwrap();
        let mut state = RevState::load(workspace, &storage).unwrap();
        let mut highlighter = PlainHighlighter;
        let paths = AppPaths {
            data: "/tmp/rev-test/data".into(),
            cache: "/tmp/rev-test/cache".into(),
            database: "/tmp/rev-test/rev.db".into(),
            roots: "/tmp/rev-test/roots".into(),
            prs: "/tmp/rev-test/prs".into(),
            exports: "/tmp/rev-test/exports".into(),
            skills: "/tmp/rev-test/skills".into(),
            plugins: "/tmp/rev-test/plugins".into(),
        };
        let count = row_count(&mut state, &mut highlighter);
        for _ in 0..(count + 20) {
            handle_review_key(
                &mut state,
                &storage,
                &paths,
                &mut highlighter,
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(state.file_index, 0);
        assert_eq!(state.row_cursor, count - 1);
        assert!(state.status.contains("press l"));
        assert_eq!(state.mode, RevMode::Normal);

        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.file_index, 1);
        assert_eq!(state.row_cursor, 0);
        state.row_cursor = 1;
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.file_index, 0);
        assert_eq!(state.row_cursor, count - 1);
        handle_review_key(
            &mut state,
            &storage,
            &paths,
            &mut highlighter,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(state.file_index, 1);
        assert_eq!(state.row_cursor, 1);
    }
}

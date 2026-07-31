use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
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

use crate::annotations::{create_local_annotation, AnnotationRequest};
use crate::chat_render::render_markdown_mapped;
use crate::config::AppPaths;
use crate::copilot::{
    start_agent, AgentCommand, AgentEvent, AgentRuntime, BridgeConfig, ModelOption, ModelSelection,
    Outbound, OutboundKind,
};
use crate::diff::{DiffFile, DiffLine, FileStatus, Hunk, LineKind};
use crate::domain::{
    Annotation, AnnotationKind, AskMessage, BaseBranchSource, DeliveryState, Placement,
};
use crate::export::CommentExport;
use crate::git::Git;
use crate::highlight::{Highlighter, PlainHighlighter, StyledSegment, SyntectHighlighter};
use crate::storage::{now, RevQuestionSession, Storage};
use crate::terminal_text::{
    cell_width, floor_grapheme_boundary, grapheme_indices, next_grapheme_boundary,
    previous_grapheme_boundary,
};
use crate::work_item::{resolve_local, ResolvedWorkItem, ReviewRepo};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_EVENTS_PER_TICK: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevMode {
    Normal,
    Visual,
    Compose,
    Command,
    Model,
    History,
    ConfirmClear,
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
    NewQuestion,
    FollowUp(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerStage {
    Model,
    Reasoning,
    Context,
}

#[derive(Clone, Debug)]
struct ModelPicker {
    models: Vec<ModelOption>,
    stage: PickerStage,
    index: usize,
    model_index: usize,
    reasoning_effort: Option<String>,
}

impl ModelPicker {
    fn new(models: Vec<ModelOption>) -> Self {
        Self {
            models,
            stage: PickerStage::Model,
            index: 0,
            model_index: 0,
            reasoning_effort: None,
        }
    }

    fn model(&self) -> &ModelOption {
        &self.models[self.model_index.min(self.models.len().saturating_sub(1))]
    }

    fn option_count(&self) -> usize {
        match self.stage {
            PickerStage::Model => self.models.len(),
            PickerStage::Reasoning => self.model().supported_reasoning_efforts.len() + 1,
            PickerStage::Context => self.model().context_tiers.len().max(1),
        }
    }

    fn selection(&self) -> ModelSelection {
        let model = self.model();
        let context_tier = match self.stage {
            PickerStage::Context => model
                .context_tiers
                .get(self.index)
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
    pending: PendingSend,
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
}

struct RenderCache {
    revision: u64,
    width: usize,
    rows: Vec<RevRow>,
}

struct RevState {
    workspace: ResolvedWorkItem,
    files: Vec<(usize, usize)>,
    file_index: usize,
    row_cursor: usize,
    visual_anchor: Option<usize>,
    mode: RevMode,
    diff_layout: RevDiffLayout,
    command: CommandPalette,
    branch_candidates: Vec<String>,
    original_context: HashSet<String>,
    compose_target: Option<ComposeTarget>,
    compose: String,
    compose_cursor: usize,
    compose_scroll: u16,
    status: String,
    annotations: Vec<(Annotation, Placement)>,
    threads: HashMap<String, Vec<AskMessage>>,
    picker: Option<ModelPicker>,
    agent: Option<RevAgentSlot>,
    queued_questions: VecDeque<QuestionLaunch>,
    streaming: bool,
    agent_activity: String,
    agent_last_event: Instant,
    rows_revision: u64,
    render_cache: Option<RenderCache>,
    history_cursor: usize,
    history_delete_armed: Option<String>,
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
        for repo in &workspace.repos {
            for pair in storage.annotations_for_version(&repo.version.id)? {
                let annotation = &pair.0;
                if annotation.kind == AnnotationKind::Ask {
                    threads.insert(
                        annotation.id.clone(),
                        storage.ask_messages_for_annotation(&annotation.id)?,
                    );
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
        Ok(Self {
            workspace,
            files,
            file_index: 0,
            row_cursor: 0,
            visual_anchor: None,
            mode: RevMode::Normal,
            diff_layout: RevDiffLayout::Unified,
            command: CommandPalette::default(),
            branch_candidates,
            original_context,
            compose_target: None,
            compose: String::new(),
            compose_cursor: 0,
            compose_scroll: 0,
            status: "j/k stay in this file · h/l change files".into(),
            annotations,
            threads,
            picker: None,
            agent: None,
            queued_questions: VecDeque::new(),
            streaming: false,
            agent_activity: "Copilot starts only when you ask a question".into(),
            agent_last_event: Instant::now(),
            rows_revision: 1,
            render_cache: None,
            history_cursor: 0,
            history_delete_armed: None,
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

    fn move_file(&mut self, forward: bool) {
        if self.files.is_empty() {
            return;
        }
        let previous = self.file_index;
        self.file_index = if forward {
            (self.file_index + 1).min(self.files.len() - 1)
        } else {
            self.file_index.saturating_sub(1)
        };
        self.row_cursor = 0;
        self.visual_anchor = None;
        self.mode = RevMode::Normal;
        self.render_cache = None;
        self.status = if previous == self.file_index {
            if forward {
                "Already at the last file".into()
            } else {
                "Already at the first file".into()
            }
        } else {
            "Changed file with h/l · j/k remain bounded here".into()
        };
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
    result
}

fn recover_pending_questions(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
) -> Result<()> {
    let pending = storage.pending_ask_messages(&state.workspace.item.id)?;
    for message in pending {
        let Some((annotation, placement)) = state
            .annotations
            .iter()
            .find(|(annotation, _)| annotation.id == message.annotation_id)
        else {
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
                pending: PendingSend {
                    annotation_id: annotation.id.clone(),
                    user_message_id: message.id,
                    assistant_message_id: Uuid::new_v4().to_string(),
                    assistant_seq: message.seq + 1,
                    prompt,
                    needs_model_picker: existing.is_none(),
                },
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
        drain_agent_events(state, storage, paths)?;
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
                        RevMode::Command => {
                            let count = command_candidates(state).len();
                            state.command.selected =
                                (state.command.selected + 1).min(count.saturating_sub(1));
                        }
                        RevMode::Model | RevMode::ConfirmClear => {}
                    },
                    MouseEventKind::ScrollUp => match state.mode {
                        RevMode::Normal | RevMode::Visual => move_row(state, highlighter, -3),
                        RevMode::Compose => {
                            state.compose_scroll = state.compose_scroll.saturating_sub(3)
                        }
                        RevMode::History => {
                            state.history_cursor = state.history_cursor.saturating_sub(3)
                        }
                        RevMode::Command => {
                            state.command.selected = state.command.selected.saturating_sub(1)
                        }
                        RevMode::Model | RevMode::ConfirmClear => {}
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
    match state.mode {
        RevMode::Compose => handle_compose_key(state, storage, paths, key),
        RevMode::Command => handle_command_key(state, storage, paths, key),
        RevMode::Model => handle_model_key(state, storage, paths, key),
        RevMode::History => handle_history_key(state, storage, key),
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
        RevMode::Normal | RevMode::Visual => {
            handle_review_key(state, storage, paths, highlighter, key)
        }
    }
}

fn handle_review_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    highlighter: &mut dyn Highlighter,
    key: KeyEvent,
) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.visual_anchor = None;
        state.mode = RevMode::Normal;
        state.status = "Selection cleared".into();
        return Ok(());
    }
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            expand_hunk_edge(state, true)?;
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            expand_hunk_edge(state, false)?;
        }
        KeyCode::Char('j') | KeyCode::Down => move_row(state, highlighter, 1),
        KeyCode::Char('k') | KeyCode::Up => move_row(state, highlighter, -1),
        KeyCode::Char('h') | KeyCode::Left => state.move_file(false),
        KeyCode::Char('l') | KeyCode::Right => state.move_file(true),
        KeyCode::Char('g') | KeyCode::Home => state.row_cursor = 0,
        KeyCode::Char('G') | KeyCode::End => {
            let count = row_count(state, highlighter);
            state.row_cursor = count.saturating_sub(1);
        }
        KeyCode::Char('v') => {
            if state.mode == RevMode::Visual {
                state.visual_anchor = None;
                state.mode = RevMode::Normal;
                state.status = "Selection cleared".into();
            } else if let Some(source) = current_source_index(state, highlighter) {
                state.visual_anchor = Some(source);
                state.mode = RevMode::Visual;
                state.status = "VISUAL · j/k selects source rows in this file".into();
            }
        }
        KeyCode::Char('a') => begin_compose(state, highlighter, ComposeTarget::NewQuestion),
        KeyCode::Char('c') => begin_compose(state, highlighter, ComposeTarget::Feedback),
        KeyCode::Char('i') | KeyCode::Enter => {
            if let Some(id) = current_annotation_id(state, highlighter) {
                if state
                    .annotation(&id)
                    .is_some_and(|(annotation, _)| annotation.kind == AnnotationKind::Ask)
                {
                    begin_compose(state, highlighter, ComposeTarget::FollowUp(id));
                }
            }
        }
        KeyCode::Char('r') => {
            state.mode = RevMode::History;
            state.history_cursor = 0;
            state.status = "HISTORY · j/k move · d delete item · Esc return".into();
        }
        KeyCode::Char('e') => copy_feedback_prompt(state, storage)?,
        KeyCode::Char('C') => {
            state.mode = RevMode::ConfirmClear;
            state.status = "Clear every saved comment and question here? y/n".into();
        }
        KeyCode::Char(':') => {
            state.mode = RevMode::Command;
            state.command = CommandPalette::default();
            state.status = "COMMAND · type to filter · ↑/↓ select · Enter run · Esc return".into();
        }
        KeyCode::Char('q') => {
            if state.agent.is_some() || !state.queued_questions.is_empty() {
                state.status =
                    "Questions are active or queued · Ctrl-C cancels the active one before quit"
                        .into();
            } else {
                state.status = "quit".into();
            }
        }
        KeyCode::Esc => {
            state.mode = RevMode::Normal;
            state.visual_anchor = None;
            state.status = "NORMAL".into();
        }
        _ => {}
    }
    let _ = paths;
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
            let command = candidates
                .get(state.command.selected)
                .filter(|_| !candidates.is_empty())
                .cloned()
                .unwrap_or_else(|| state.command.input.trim().to_owned());
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
        "history".to_owned(),
        "clear".to_owned(),
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
        "export feedback" | "feedback" | "export" => {
            state.mode = RevMode::Normal;
            copy_feedback_prompt(state, storage)?;
        }
        "history" => {
            state.mode = RevMode::History;
            state.history_cursor = 0;
            state.status = "HISTORY · j/k move · d delete item · Esc return".into();
        }
        "clear" => {
            state.mode = RevMode::ConfirmClear;
            state.status = "Clear every saved comment and question here? y/n".into();
        }
        "quit" | "q" => state.status = "quit".into(),
        _ if command.starts_with("base ") => {
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
    let revision = repo.version.head_sha.clone();
    let file_path = file.display_path.clone();
    let working = std::fs::read_to_string(repo_path.join(&file_path))
        .or_else(|_| Git::default().file_at_revision(&repo_path, &revision, &file_path))?;
    let old = Git::default()
        .file_at_revision(
            &repo_path,
            &revision,
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
        hunk.lines.retain(|line| line.kind != LineKind::Meta);
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
                content: format!(
                    "··· {gap} unchanged lines ··· (Shift+↑/↓ reveals 5 at the active hunk edge)"
                ),
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

fn handle_compose_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            let retain_selection = state.compose.trim().is_empty()
                && state.visual_anchor.is_some()
                && matches!(
                    state.compose_target,
                    Some(ComposeTarget::Feedback | ComposeTarget::NewQuestion)
                );
            state.mode = if retain_selection {
                RevMode::Visual
            } else {
                RevMode::Normal
            };
            state.compose_target = None;
            state.compose.clear();
            state.compose_cursor = 0;
            state.compose_scroll = 0;
            state.status = if retain_selection {
                "VISUAL · empty box closed · selection retained · Esc clears it".into()
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
        KeyCode::Enter => submit_compose(state, storage, paths),
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
    state.invalidate_rows();
    state.status =
        "Feedback saved · e copies a structured prompt · `rev feedback` prints it".into();
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
            pending: PendingSend {
                annotation_id,
                user_message_id: user.id,
                assistant_message_id: Uuid::new_v4().to_string(),
                assistant_seq: 1,
                prompt,
                needs_model_picker: true,
            },
            existing: None,
        },
    );
    state.visual_anchor = None;
    Ok(())
}

fn create_follow_up(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    annotation_id: &str,
    text: &str,
) -> Result<()> {
    let existing = storage
        .rev_question_session(annotation_id)?
        .context("this question has no isolated Copilot session to resume")?;
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
    storage.append_ask_message(&user)?;
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
            pending: PendingSend {
                annotation_id: annotation_id.to_owned(),
                user_message_id: user.id,
                assistant_message_id: Uuid::new_v4().to_string(),
                assistant_seq: seq + 1,
                prompt: text.to_owned(),
                needs_model_picker: false,
            },
            existing: Some(existing),
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
    let selection = launch
        .existing
        .as_ref()
        .map(|session| ModelSelection {
            model_id: session.model_id.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
            context_tier: session.context_tier.clone(),
        })
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
        model: selection.model_id.clone(),
        reasoning_effort: selection.reasoning_effort.clone(),
        context_tier: selection.context_tier.clone(),
        skill_directories,
        plugin_directories,
    });
    state.agent = Some(RevAgentSlot {
        question_id: launch.annotation_id,
        runtime,
        pending: Some(launch.pending.clone()),
        outbound_id: None,
        session_id: launch
            .existing
            .as_ref()
            .map(|session| session.session_id.clone()),
        selection,
        needs_model_picker: launch.pending.needs_model_picker,
    });
    state.agent_activity = if launch.existing.is_some() {
        "Resuming this question's Copilot session…".into()
    } else {
        "Creating a new Copilot session for this question…".into()
    };
    state.agent_last_event = Instant::now();
    state.status = "Question session starting in the background · input remains responsive".into();
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
            agent.session_id = Some(session_id.clone());
            let timestamp = now();
            let existing = storage.rev_question_session(&agent.question_id)?;
            storage.upsert_rev_question_session(&RevQuestionSession {
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
            })?;
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
                anyhow::bail!("Copilot returned no selectable models");
            }
            state.picker = Some(ModelPicker::new(models));
            state.mode = RevMode::Model;
            state.status = "MODEL 1/3 · choose a model for this question".into();
        }
        AgentEvent::ModelSelectionChanged(selection) => {
            let agent = state
                .agent
                .as_mut()
                .context("model changed without an active question")?;
            agent.selection = selection.clone();
            agent.needs_model_picker = false;
            let session_id = agent
                .session_id
                .clone()
                .context("model changed before session creation")?;
            let timestamp = now();
            let existing = storage.rev_question_session(&agent.question_id)?;
            storage.upsert_rev_question_session(&RevQuestionSession {
                annotation_id: agent.question_id.clone(),
                session_id,
                model_id: selection.model_id,
                reasoning_effort: selection.reasoning_effort,
                context_tier: selection.context_tier,
                state: "ready".into(),
                created_at: existing
                    .as_ref()
                    .map(|session| session.created_at.clone())
                    .unwrap_or_else(|| timestamp.clone()),
                updated_at: timestamp,
            })?;
            if let Some(pending) = agent.pending.take() {
                send_pending(agent, pending)?;
                state.streaming = true;
                state.mode = RevMode::Normal;
                state.status = "Question sent in its own selected-model session".into();
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
            state.streaming = false;
            state.agent_activity = format!("Copilot error: {message}");
            state.status = state.agent_activity.clone();
            finish_active_question(state, paths);
        }
        AgentEvent::Stopped => {
            if state.streaming {
                state.streaming = false;
                state.status = "Copilot stopped before the answer completed".into();
            }
            finish_active_question(state, paths);
        }
        AgentEvent::HistoryLoaded(_)
        | AgentEvent::Queued { .. }
        | AgentEvent::QueueCancelled { .. }
        | AgentEvent::QueueReplaced { .. }
        | AgentEvent::QueueReplaceRejected { .. }
        | AgentEvent::SteeringAccepted { .. }
        | AgentEvent::SteeringFailed { .. }
        | AgentEvent::StopSettledAlreadyIdle
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
    state.agent.take();
    state.picker = None;
    state.mode = RevMode::Normal;
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
    let picker = state.picker.as_mut().context("model mode has no picker")?;
    match key.code {
        KeyCode::Esc => {
            if let Some(message_id) = state
                .agent
                .as_ref()
                .and_then(|agent| agent.pending.as_ref())
                .map(|pending| pending.user_message_id.clone())
            {
                storage.discard_pending_ask(&message_id)?;
            }
            state.mode = RevMode::Normal;
            state.agent.take();
            state.picker = None;
            state.status = "Question kept as a draft; model selection cancelled".into();
            if let Some(next) = state.queued_questions.pop_front() {
                start_question_agent(state, paths, next);
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            picker.index = (picker.index + 1).min(picker.option_count().saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => picker.index = picker.index.saturating_sub(1),
        KeyCode::Enter => match picker.stage {
            PickerStage::Model => {
                picker.model_index = picker.index;
                picker.index = 0;
                picker.stage = PickerStage::Reasoning;
                state.status = "MODEL 2/3 · choose thinking level".into();
            }
            PickerStage::Reasoning => {
                picker.reasoning_effort = picker
                    .index
                    .checked_sub(1)
                    .and_then(|index| picker.model().supported_reasoning_efforts.get(index))
                    .cloned();
                picker.index = 0;
                picker.stage = PickerStage::Context;
                state.status = "MODEL 3/3 · choose context window".into();
            }
            PickerStage::Context => {
                let selection = picker.selection();
                let agent = state
                    .agent
                    .as_ref()
                    .context("picker has no question agent")?;
                agent
                    .runtime
                    .send(AgentCommand::SelectModel(selection.clone()))?;
                state.status = format!("Applying {} to this question session…", selection.model_id);
                let _ = storage;
            }
        },
        _ => {}
    }
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
        KeyCode::Char('d') => {
            if state.agent.is_some() || !state.queued_questions.is_empty() {
                state.status =
                    "Finish or cancel active and queued questions before deleting history".into();
                return Ok(());
            }
            if let Some((annotation, _)) = state.annotations.get(state.history_cursor).cloned() {
                if state.history_delete_armed.as_deref() != Some(&annotation.id) {
                    state.history_delete_armed = Some(annotation.id);
                    state.status =
                        "Press d again to permanently delete this saved review item".into();
                    return Ok(());
                }
                storage.delete_annotation(&annotation.id)?;
                state.annotations.remove(state.history_cursor);
                state.threads.remove(&annotation.id);
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

fn copy_feedback_prompt(state: &mut RevState, storage: &Storage) -> Result<()> {
    let export = CommentExport::load(storage, &state.workspace.item)?;
    if export.comments.is_empty() {
        state.status = "No feedback to copy".into();
        return Ok(());
    }
    let prompt = export.structured_agent_prompt();
    let encoded = base64::engine::general_purpose::STANDARD.encode(prompt.as_bytes());
    print!("\x1b]52;c;{encoded}\x07");
    state.status = format!(
        "Copied {} feedback item(s) · `rev feedback` also prints the prompt",
        export.comments.len()
    );
    Ok(())
}

fn move_row(state: &mut RevState, highlighter: &mut dyn Highlighter, delta: isize) {
    let width = cached_row_width(state);
    let rows = ensure_rows(state, width, highlighter).to_vec();
    if rows.is_empty() {
        return;
    }
    if state.mode == RevMode::Visual {
        let direction = delta.signum();
        let mut cursor = state.row_cursor;
        for _ in 0..delta.unsigned_abs().max(1) {
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
    state.status = if state.row_cursor == 0 && delta < 0 {
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
            annotation.repo_id == repo.record.id
                && annotation.file_path.to_string_lossy() == file_path
        })
        .collect::<Vec<_>>();
    annotations.sort_by_key(|(_, placement)| (placement.line_end, placement.line_start));
    let mut rows = Vec::new();
    for (visible_index, line) in file.visible_lines().enumerate() {
        rows.push(RevRow {
            kind: RevRowKind::Source {
                visible_index,
                line: line.clone(),
            },
            line: None,
        });
        let source_line = line.new_line.or(line.old_line).unwrap_or(0) as i64;
        for (annotation, placement) in annotations
            .iter()
            .filter(|(_, placement)| placement.line_end == source_line)
        {
            let title = match annotation.kind {
                AnnotationKind::Comment => format!(
                    "Feedback · lines {}-{} · {}",
                    placement.line_start,
                    placement.line_end,
                    annotation.text.as_deref().unwrap_or_default()
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
                        rows.push(RevRow {
                            kind: RevRowKind::Annotation {
                                annotation_id: annotation.id.clone(),
                                anchor_visible_index: visible_index,
                            },
                            line: Some(line),
                        });
                    }
                }
                rows.push(RevRow {
                    kind: RevRowKind::Annotation {
                        annotation_id: annotation.id.clone(),
                        anchor_visible_index: visible_index,
                    },
                    line: Some(Line::styled(
                        "╰─ i/Enter follow up",
                        Style::default().fg(Color::DarkGray),
                    )),
                });
            } else {
                rows.push(RevRow {
                    kind: RevRowKind::Annotation {
                        annotation_id: annotation.id.clone(),
                        anchor_visible_index: visible_index,
                    },
                    line: Some(Line::styled(
                        "╰─ saved · e copy structured prompt",
                        Style::default().fg(Color::DarkGray),
                    )),
                });
            }
        }
    }
    rows
}

fn render(frame: &mut Frame, state: &mut RevState, highlighter: &mut dyn Highlighter) {
    let area = frame.area();
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
    if state.mode == RevMode::History {
        render_history(frame, state, vertical[1]);
    } else {
        render_review(frame, state, vertical[1], highlighter);
    }
    if state.mode == RevMode::Compose {
        render_composer(frame, state, vertical[2]);
    }
    render_footer(frame, state, vertical[3]);
    if state.mode == RevMode::Model {
        render_model_picker(frame, state, vertical[1]);
    } else if state.mode == RevMode::Command {
        render_command_palette(frame, state, vertical[1]);
    } else if state.mode == RevMode::ConfirmClear {
        render_confirmation(frame, vertical[1]);
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
    let border = if state.mode == RevMode::Visual {
        Color::Magenta
    } else {
        Color::Cyan
    };
    let block = Block::default()
        .title(format!(
            " review · {} · j/k bounded · h/l files · Shift+↑/↓ expand ",
            state.diff_layout.label()
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width.max(1) as usize;
    let rows = ensure_rows(state, width, highlighter).to_vec();
    state.clamp_cursor(rows.len());
    let viewport = inner.height.max(1) as usize;
    let start = state
        .row_cursor
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(rows.len().saturating_sub(viewport));
    let selection = state
        .visual_anchor
        .zip(current_source_index_from_rows(&rows, state.row_cursor))
        .map(|(a, b)| (a.min(b), a.max(b)));
    let mut visible = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(start).take(viewport) {
        let selected = index == state.row_cursor;
        let mut line = match &row.kind {
            RevRowKind::Source {
                visible_index,
                line,
            } => match state.diff_layout {
                RevDiffLayout::Unified => source_line(
                    state.current_file().map(DiffFile::path),
                    line,
                    *visible_index,
                    width,
                    highlighter,
                ),
                RevDiffLayout::Split => split_source_line(
                    state.current_file().map(DiffFile::path),
                    line,
                    *visible_index,
                    width,
                    highlighter,
                ),
            },
            RevRowKind::Annotation { .. } => row.line.clone().unwrap_or_default(),
        };
        let expanded_context = matches!(
            &row.kind,
            RevRowKind::Source { line, .. } if is_expanded_context(state, line)
        );
        let visually_selected = match &row.kind {
            RevRowKind::Source { visible_index, .. } => {
                selection.is_some_and(|(start, end)| (start..=end).contains(visible_index))
            }
            RevRowKind::Annotation { .. } => false,
        };
        if selected || visually_selected {
            line = line.style(Style::default().bg(if state.mode == RevMode::Visual {
                Color::Rgb(52, 35, 63)
            } else {
                Color::Rgb(35, 45, 58)
            }));
        } else if expanded_context {
            line = line.style(Style::default().bg(Color::Rgb(38, 38, 38)));
        }
        visible.push(line);
    }
    frame.render_widget(Paragraph::new(Text::from(visible)), inner);
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

fn source_line(
    path: Option<&Path>,
    line: &DiffLine,
    visible_index: usize,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Line<'static> {
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
    let segments = path
        .and_then(|path| {
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
        });
    let mut spans = vec![Span::styled(rendered_gutter, gutter_style)];
    let mut used = 0usize;
    'segments: for segment in segments {
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
        for (_, grapheme) in grapheme_indices(&segment.text) {
            let grapheme_width = cell_width(grapheme);
            if used.saturating_add(grapheme_width) > available {
                spans.push(Span::styled("…", Style::default().fg(Color::Yellow)));
                break 'segments;
            }
            spans.push(Span::styled(grapheme.to_owned(), style));
            used += grapheme_width;
        }
    }
    Line::from(spans)
}

fn split_source_line(
    path: Option<&Path>,
    line: &DiffLine,
    visible_index: usize,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Line<'static> {
    let left_width = width.saturating_sub(1) / 2;
    let right_width = width.saturating_sub(left_width + 1);
    let left = match line.kind {
        LineKind::Addition => blank_side(left_width),
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
        LineKind::Deletion => blank_side(right_width),
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
    let mut spans = left;
    spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
    spans.extend(right);
    Line::from(spans)
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
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
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
    let segments = path
        .and_then(|path| {
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
        });
    let mut spans = vec![Span::styled(rendered_gutter, gutter_style)];
    let mut content_used = 0usize;
    'segments: for segment in segments {
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
        for (_, grapheme) in grapheme_indices(&segment.text) {
            let grapheme_width = cell_width(grapheme);
            if content_used.saturating_add(grapheme_width) > available {
                if available > 0 {
                    spans.push(Span::styled("…", Style::default().fg(Color::Yellow)));
                }
                break 'segments;
            }
            spans.push(Span::styled(grapheme.to_owned(), style));
            content_used += grapheme_width;
        }
    }
    let current_width: usize = spans
        .iter()
        .map(|span| cell_width(span.content.as_ref()))
        .sum();
    if current_width < width {
        spans.push(Span::raw(" ".repeat(width - current_width)));
    }
    spans
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
        RevMode::ConfirmClear => "CONFIRM",
    };
    let first = if area.width < 80 {
        format!("{mode} · {}", state.status)
    } else {
        format!(
            "{mode} · {} · : commands · a ask · c feedback · r history · e copy · q quit",
            state.status
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
    let popup = centered_rect(
        area,
        72.min(area.width.saturating_sub(2)),
        14.min(area.height),
    );
    frame.render_widget(Clear, popup);
    let (title, options) = match picker.stage {
        PickerStage::Model => (
            " model 1/3 ",
            picker
                .models
                .iter()
                .map(|model| {
                    let context = model
                        .max_context_tokens
                        .map(|tokens| format!(" · {tokens} ctx"))
                        .unwrap_or_default();
                    format!("{}{}", model.name, context)
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
            if picker.model().context_tiers.is_empty() {
                vec!["Runtime default".into()]
            } else {
                picker
                    .model()
                    .context_tiers
                    .iter()
                    .map(|tier| {
                        tier.max_context_tokens
                            .map(|tokens| format!("{} · {tokens} tokens", tier.id))
                            .unwrap_or_else(|| tier.id.clone())
                    })
                    .collect()
            },
        ),
    };
    let block = Block::default()
        .title(format!("{title} · ↑/↓ scroll · Enter deeper "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    let viewport = inner.height.max(1) as usize;
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
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_history(frame: &mut Frame, state: &RevState, area: Rect) {
    let block = Block::default()
        .title(" persisted review history · d deletes selected ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let viewport = inner.height as usize;
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
                        "{} {}:{}-{} · {} · {}",
                        if index == state.history_cursor {
                            "❯"
                        } else {
                            " "
                        },
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
    frame.render_widget(Paragraph::new(rows), inner);
}

fn render_confirmation(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(area, 62.min(area.width), 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new("Clear all persisted comments and questions for this workspace?\n\ny = clear · n/Esc = cancel")
            .block(
                Block::default()
                    .title(" confirm clear ")
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
        "history" => state.mode = RevMode::History,
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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    use super::{
        handle_compose_key, handle_review_key, merge_touching_hunks, queue_question_launch, render,
        render_snapshot, row_count, snapshot_workspace, ComposeTarget, PendingSend, QuestionLaunch,
        RevAgentSlot, RevMode, RevState,
    };
    use crate::config::AppPaths;
    use crate::copilot::{AgentCommand, AgentEvent, AgentRuntime, AgentSink, ModelSelection};
    use crate::diff::{DiffLine, Hunk, LineKind};
    use crate::highlight::PlainHighlighter;
    use crate::storage::Storage;

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

        let command = render_snapshot(100, 28, "command").unwrap();
        assert!(command.contains("command palette"));
        assert!(command.contains("❯ diff split"));

        let composer = render_snapshot(100, 28, "composer").unwrap();
        assert!(composer.contains("rows 13-30 of 30"));
        assert!(composer.contains("↑/↓ scroll"));
        assert!(composer.contains("COPILOT"));
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
                pending: PendingSend {
                    annotation_id: "second".into(),
                    user_message_id: "user".into(),
                    assistant_message_id: "assistant".into(),
                    assistant_seq: 1,
                    prompt: "question".into(),
                    needs_model_picker: true,
                },
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
    }
}

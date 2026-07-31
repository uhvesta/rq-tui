use std::collections::{HashMap, VecDeque};
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
use crate::diff::{DiffFile, DiffLine, LineKind};
use crate::domain::{Annotation, AnnotationKind, AskMessage, DeliveryState, Placement};
use crate::export::CommentExport;
use crate::git::Git;
use crate::highlight::{Highlighter, PlainHighlighter, StyledSegment, SyntectHighlighter};
use crate::storage::{now, RevQuestionSession, Storage};
use crate::terminal_text::{
    cell_width, floor_grapheme_boundary, grapheme_indices, next_grapheme_boundary,
    previous_grapheme_boundary,
};
use crate::work_item::{ResolvedWorkItem, ReviewRepo};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_EVENTS_PER_TICK: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevMode {
    Normal,
    Visual,
    Compose,
    Model,
    History,
    ConfirmClear,
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
    compose_target: Option<ComposeTarget>,
    compose: String,
    compose_cursor: usize,
    status: String,
    annotations: Vec<(Annotation, Placement)>,
    threads: HashMap<String, Vec<AskMessage>>,
    picker: Option<ModelPicker>,
    agent: Option<RevAgentSlot>,
    queued_questions: VecDeque<QuestionLaunch>,
    streaming: bool,
    agent_activity: String,
    agent_last_event: Instant,
    quit_armed: bool,
    rows_revision: u64,
    render_cache: Option<RenderCache>,
    history_cursor: usize,
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
        Ok(Self {
            workspace,
            files,
            file_index: 0,
            row_cursor: 0,
            visual_anchor: None,
            mode: RevMode::Normal,
            compose_target: None,
            compose: String::new(),
            compose_cursor: 0,
            status: "j/k stay in this file · h/l change files".into(),
            annotations,
            threads,
            picker: None,
            agent: None,
            queued_questions: VecDeque::new(),
            streaming: false,
            agent_activity: "Copilot starts only when you ask a question".into(),
            agent_last_event: Instant::now(),
            quit_armed: false,
            rows_revision: 1,
            render_cache: None,
            history_cursor: 0,
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
                    handle_key(state, storage, paths, highlighter, key)?;
                }
                Event::Paste(text) if state.mode == RevMode::Compose => {
                    state.compose.insert_str(state.compose_cursor, &text);
                    state.compose_cursor += text.len();
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown => move_row(state, highlighter, 3),
                    MouseEventKind::ScrollUp => move_row(state, highlighter, -3),
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
    match state.mode {
        RevMode::Compose => handle_compose_key(state, storage, paths, key),
        RevMode::Model => handle_model_key(state, storage, key),
        RevMode::History => handle_history_key(state, storage, key),
        RevMode::ConfirmClear => {
            match key.code {
                KeyCode::Char('y') => {
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
        if state.streaming {
            if let Some(agent) = state.agent.as_ref() {
                agent.runtime.send(AgentCommand::Abort)?;
                state.agent_activity = "Cancelling the active question…".into();
                state.agent_last_event = Instant::now();
            }
        } else {
            state.visual_anchor = None;
            state.mode = RevMode::Normal;
            state.status = "Selection cleared".into();
        }
        return Ok(());
    }
    match key.code {
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
        KeyCode::Char('q') => {
            if state.streaming && !state.quit_armed {
                state.quit_armed = true;
                state.status =
                    "A question is active · Ctrl-C cancels · press q again to quit anyway".into();
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
    if matches!(target, ComposeTarget::Feedback | ComposeTarget::NewQuestion)
        && selected_source_range(state, highlighter).is_none()
    {
        state.status = "Move to a source row before adding feedback or a question".into();
        return;
    }
    state.mode = RevMode::Compose;
    state.compose_target = Some(target);
    state.compose.clear();
    state.compose_cursor = 0;
    state.status = "INSERT · Enter submit · Shift-Enter newline · Esc cancel".into();
}

fn handle_compose_key(
    state: &mut RevState,
    storage: &Storage,
    paths: &AppPaths,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            state.mode = RevMode::Normal;
            state.compose_target = None;
            state.status = "Draft cancelled".into();
            Ok(())
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.compose.insert(state.compose_cursor, '\n');
            state.compose_cursor += 1;
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
            }
            Ok(())
        }
        KeyCode::Delete => {
            let next = next_grapheme_boundary(&state.compose, state.compose_cursor);
            if next > state.compose_cursor {
                state.compose.replace_range(state.compose_cursor..next, "");
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
            Ok(())
        }
        KeyCode::End => {
            state.compose_cursor = state.compose.len();
            Ok(())
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.compose.insert(state.compose_cursor, character);
            state.compose_cursor += character.len_utf8();
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
    if state.streaming {
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
    state.agent.take();
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
        handle_agent_event(state, storage, paths, event)?;
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
            append_response(state, &outbound_id, &delta, false);
        }
        AgentEvent::ResponseSnapshot { outbound_id, text } => {
            append_response(state, &outbound_id, &text, true);
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
            state.quit_armed = false;
            if let Some(agent) = state.agent.as_mut() {
                agent.outbound_id = None;
            }
            if let Some(next) = state.queued_questions.pop_front() {
                start_question_agent(state, paths, next);
            }
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
        }
        AgentEvent::Stopped => {
            if state.streaming {
                state.streaming = false;
                state.status = "Copilot stopped before the answer completed".into();
            }
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

fn append_response(state: &mut RevState, outbound_id: &str, text: &str, snapshot: bool) {
    let Some(agent) = state.agent.as_ref() else {
        return;
    };
    if agent.outbound_id.as_deref() != Some(outbound_id) {
        return;
    }
    let Some(pending) = agent.pending.as_ref() else {
        return;
    };
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
            message.text.clone_from(&text.to_owned());
        } else {
            message.text.push_str(text);
        }
        state.invalidate_rows();
    }
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

fn handle_model_key(state: &mut RevState, storage: &Storage, key: KeyEvent) -> Result<()> {
    let picker = state.picker.as_mut().context("model mode has no picker")?;
    match key.code {
        KeyCode::Esc => {
            state.mode = RevMode::Normal;
            state.agent.take();
            state.status = "Question kept as a draft; model selection cancelled".into();
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
            state.status = "Back to review".into();
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.history_cursor =
                (state.history_cursor + 1).min(state.annotations.len().saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.history_cursor = state.history_cursor.saturating_sub(1);
        }
        KeyCode::Char('d') => {
            if let Some((annotation, _)) = state.annotations.get(state.history_cursor).cloned() {
                storage.delete_annotation(&annotation.id)?;
                state.annotations.remove(state.history_cursor);
                state.threads.remove(&annotation.id);
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
    let rows = ensure_rows(state, 100, highlighter).to_vec();
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
    ensure_rows(state, 100, highlighter).len()
}

fn current_source_index(state: &mut RevState, highlighter: &mut dyn Highlighter) -> Option<usize> {
    let cursor = state.row_cursor;
    match &ensure_rows(state, 100, highlighter).get(cursor)?.kind {
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
    match &ensure_rows(state, 100, highlighter).get(cursor)?.kind {
        RevRowKind::Annotation { annotation_id, .. } => Some(annotation_id.clone()),
        RevRowKind::Source { .. } => None,
    }
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
        composer_height(&state.compose, area.width.saturating_sub(4) as usize)
    } else {
        0
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(compose_height),
            Constraint::Length(2),
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
        " rev · {} · {} > {} · file {}/{} ",
        state.workspace.item.name,
        repo,
        file,
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
        .title(" review · j/k bounded · h/l files ")
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
            } => source_line(
                state.current_file().map(DiffFile::path),
                line,
                *visible_index,
                width,
                highlighter,
            ),
            RevRowKind::Annotation { .. } => row.line.clone().unwrap_or_default(),
        };
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
        }
        visible.push(line);
    }
    frame.render_widget(Paragraph::new(Text::from(visible)), inner);
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
    let available = width.saturating_sub(cell_width(&gutter));
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
    let mut spans = vec![Span::styled(gutter, gutter_style)];
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

fn render_composer(frame: &mut Frame, state: &RevState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let mut text = state.compose.clone();
    let cursor = floor_grapheme_boundary(&text, state.compose_cursor);
    text.insert(cursor, '▏');
    let title = match state.compose_target {
        Some(ComposeTarget::Feedback) => " feedback ",
        Some(ComposeTarget::NewQuestion) => " question ",
        Some(ComposeTarget::FollowUp(_)) => " follow-up ",
        None => " input ",
    };
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
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
        RevMode::Model => "MODEL",
        RevMode::History => "HISTORY",
        RevMode::ConfirmClear => "CONFIRM",
    };
    let first = if area.width < 80 {
        format!("{mode} · {}", state.status)
    } else {
        format!(
            "{mode} · {} · a ask · c feedback · r history · e copy · q quit",
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
    let lines = options
        .iter()
        .enumerate()
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
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        popup,
    );
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

fn composer_height(text: &str, width: usize) -> u16 {
    let width = width.max(1);
    let cells = text
        .split('\n')
        .map(|line| cell_width(line).max(1).div_ceil(width))
        .sum::<usize>();
    (cells + 2).clamp(3, 10) as u16
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

pub(crate) fn render_snapshot(width: u16, height: u16) -> Result<String> {
    let storage = Storage::in_memory()?;
    let workspace = snapshot_workspace(&storage)?;
    let mut state = RevState::load(workspace, &storage)?;
    let mut highlighter = PlainHighlighter;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| render(frame, &mut state, &mut highlighter))?;
    Ok(terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .as_bytes()
        .chunks(width as usize)
        .map(|row| String::from_utf8_lossy(row).trim_end().to_owned())
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
        "@@ -1,3 +1,4 @@\n",
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

    use super::{
        handle_review_key, render_snapshot, row_count, snapshot_workspace, RevMode, RevState,
    };
    use crate::config::AppPaths;
    use crate::highlight::PlainHighlighter;
    use crate::storage::Storage;

    #[test]
    fn snapshot_exposes_the_small_rev_surface() {
        let snapshot = render_snapshot(100, 24).unwrap();
        assert!(snapshot.contains("rev · workspace"));
        assert!(snapshot.contains("j/k bounded"));
        assert!(snapshot.contains("h/l files"));
        assert!(snapshot.contains("a ask"));
        assert!(snapshot.contains("c feedback"));
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

use std::io;
use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use base64::Engine as _;
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;

use crate::annotations::{
    anchor_from_diff, create_local_annotation, create_snapshot, self_contained_ask,
    AnnotationRequest,
};
use crate::app::{
    command_matches, AgentPhase, AppState, ChatEntry, ComposeTarget, DiffLayout, Effect, Focus,
    InputMode, MarkdownPreview, ModelPickerStage, PendingPrune, PruneChoice, ReadyPrune,
    ReviewRowSelection, Screen, VersionChoice, INLINE_COMPOSER_ID,
};
use crate::chat_render::{render_markdown_mapped, CellSource, MappedMarkdown, MappedRow};
use crate::chat_selection::{
    BlockId, ChatBlock, ChatCell, ChatLayout, ChatMessage, ChatRow, RowBreak,
    SourceRange as SelectionSourceRange,
};
use crate::config::AppPaths;
use crate::copilot::{
    start_agent, ActivityKind, AgentCommand, AgentEvent, AgentEventEnvelope, AgentRuntime,
    AgentSink, BridgeConfig, LaneEvent, Outbound, OutboundKind, PruneSessionOutcome,
};
use crate::diff::{DiffFile, DiffLine, FileStatus, LineKind};
use crate::domain::{
    AnchorSide, AnnotationKind, AskMessage, BaseBranchSource, DeliveryState, PendingChat,
    Placement, ReviewContext, SessionRecord,
};
use crate::export::{CommentExport, ExportFormat};
use crate::git::Git;
use crate::highlight::{Highlighter, StyledSegment, SyntectHighlighter};
use crate::markdown::{escape_html, render_inline_html};
use crate::remote::{PrReference, RemoteResolver};
use crate::review_stream::{AnnotationRowPart, ReviewRow};
use crate::storage::{now, Storage};
use crate::terminal_text::{cell_width, floor_grapheme_boundary, grapheme_indices};
use crate::work_item::{combine_resolved, resolve_local};

fn load_ui_preferences(state: &mut AppState, storage: &Storage, paths: &AppPaths) -> Result<()> {
    state.expand_step = storage
        .setting("diff.expand_step")?
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|step| *step > 0)
        .unwrap_or(10);
    state.default_layout = match storage.setting("diff.layout.default")?.as_deref() {
        Some("split") => DiffLayout::Split,
        _ => DiffLayout::Unified,
    };
    state.layout = state.default_layout;
    state.file_tree_default_open = !matches!(
        storage.setting("file_tree.default")?.as_deref(),
        Some("closed")
    );
    state.picker_open = state.file_tree_default_open;
    state.markdown_preview = match storage.setting("markdown.preview")?.as_deref() {
        Some("browser") => MarkdownPreview::Browser,
        _ => MarkdownPreview::Inline,
    };
    state.cache_directory = paths.prs.display().to_string();
    state.storage_path = paths.database.display().to_string();
    Ok(())
}

fn work_item_model_key(work_item_id: &str, field: &str) -> String {
    format!("work_item.{work_item_id}.model{field}")
}

fn load_model_preferences(state: &mut AppState, storage: &Storage) -> Result<()> {
    let work_item_id = &state.work_item.item.id;
    state.model = storage
        .setting(&work_item_model_key(work_item_id, ""))?
        .or(storage.setting("model")?)
        .unwrap_or_else(|| "gpt-5".to_owned());
    state.reasoning_effort = storage
        .setting(&work_item_model_key(work_item_id, ".reasoning_effort"))?
        .or(storage.setting("model.reasoning_effort")?)
        .filter(|value| !value.trim().is_empty());
    state.context_tier = storage
        .setting(&work_item_model_key(work_item_id, ".context_tier"))?
        .or(storage.setting("model.context_tier")?)
        .filter(|value| !value.trim().is_empty());
    Ok(())
}

fn persist_model_preferences(
    state: &AppState,
    storage: &Storage,
    selection: &crate::copilot::ModelSelection,
) -> Result<()> {
    let work_item_id = &state.work_item.item.id;
    for (field, value) in [
        ("", selection.model_id.as_str()),
        (
            ".reasoning_effort",
            selection.reasoning_effort.as_deref().unwrap_or(""),
        ),
        (
            ".context_tier",
            selection.context_tier.as_deref().unwrap_or(""),
        ),
    ] {
        storage.set_setting(&work_item_model_key(work_item_id, field), value)?;
        storage.set_setting(&format!("model{field}"), value)?;
    }
    Ok(())
}

pub(crate) fn run(
    mut state: AppState,
    storage: &Storage,
    paths: &AppPaths,
    mut startup: crate::cli::StartupProgress,
) -> Result<()> {
    startup.stage("Loading review history and preferences");
    for repo in &state.work_item.repos {
        state
            .annotations
            .extend(storage.annotations_for_version(&repo.version.id)?);
    }
    for (annotation, _) in &state.annotations {
        if annotation.kind == AnnotationKind::Ask {
            state.ask_threads.insert(
                annotation.id.clone(),
                storage.ask_messages_for_annotation(&annotation.id)?,
            );
        }
    }
    if let Some(context) = storage.context_for_work_item(&state.work_item.item.id)? {
        state.context_draft = render_review_context(&context);
    } else if state
        .work_item
        .repos
        .iter()
        .all(|repo| repo.record.remote_pr_url.is_none())
    {
        state.status = "Local review has no structured context · use :generate-context".into();
    }
    load_version_choices(&mut state, storage)?;
    prompt_unseen_remote_version(&mut state);
    state.pending_asks = storage.pending_ask_messages(&state.work_item.item.id)?;
    state.pending_chats = storage.pending_chats(&state.work_item.item.id)?;
    state.pending_comment_ids = storage.pending_comment_delivery_ids(&state.work_item.item.id)?;
    state.pending_context = storage
        .context_for_work_item(&state.work_item.item.id)?
        .is_some_and(|context| context.delivery_state == DeliveryState::Pending);
    let pending_deliveries = state.pending_asks.len()
        + state.pending_chats.len()
        + usize::from(!state.pending_comment_ids.is_empty())
        + usize::from(state.pending_context);
    if pending_deliveries > 0 {
        state.previous_screen = Screen::Review;
        state.screen = Screen::Recovery;
        state.status =
            format!("{pending_deliveries} outbound message(s) may not have been delivered");
    }
    load_model_preferences(&mut state, storage)?;
    let model = state.model.clone();
    load_ui_preferences(&mut state, storage, paths)?;
    let existing_session_id = storage
        .active_session(&state.work_item.item.id)?
        .map(|session| session.id);
    let mut skill_directories = vec![paths.skills.clone()];
    skill_directories.extend(state.work_item.repos.iter().filter_map(|repo| {
        let root = repo
            .version
            .worktree_path
            .as_deref()
            .unwrap_or(&repo.record.path);
        let skills = root.join(".rq-tui").join("skills");
        skills.is_dir().then_some(skills)
    }));
    state.skill_directories = skill_directories
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut plugin_directories = vec![paths.plugins.clone()];
    plugin_directories.extend(state.work_item.repos.iter().filter_map(|repo| {
        let root = repo
            .version
            .worktree_path
            .as_deref()
            .unwrap_or(&repo.record.path);
        let plugins = root.join(".rq-tui").join("plugins");
        plugins.is_dir().then_some(plugins)
    }));
    startup.stage("Starting Copilot session");
    let bridge = start_agent(BridgeConfig {
        work_item_id: state.work_item.item.id.clone(),
        session_root: state.work_item.session_root.clone(),
        database_path: paths.database.clone(),
        app_paths: paths.clone(),
        existing_session_id,
        model,
        reasoning_effort: state.reasoning_effort.clone(),
        context_tier: state.context_tier.clone(),
        skill_directories,
        plugin_directories,
    });
    startup.finish();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    ) {
        disable_raw_mode().ok();
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        )
        .ok();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            disable_raw_mode().ok();
            execute!(
                io::stdout(),
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen,
                Show
            )
            .ok();
            return Err(error.into());
        }
    };
    let mut highlighter = SyntectHighlighter::default();

    let previous_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|panic_info| {
        disable_raw_mode().ok();
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        )
        .ok();
        eprintln!("{panic_info}");
    }));
    let result = run_loop(
        &mut terminal,
        &mut state,
        storage,
        paths,
        bridge.as_ref(),
        &mut highlighter,
    );
    std::panic::set_hook(previous_panic_hook);

    let raw_result = disable_raw_mode();
    let screen_result = execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let cursor_result = terminal.show_cursor();
    result?;
    raw_result.context("cannot disable terminal raw mode")?;
    screen_result.context("cannot leave terminal alternate screen")?;
    cursor_result.context("cannot restore terminal cursor")?;
    Ok(())
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    storage: &Storage,
    paths: &AppPaths,
    bridge: &dyn AgentRuntime,
    highlighter: &mut dyn Highlighter,
) -> Result<()> {
    let mut redraw = true;
    let mut next_periodic_redraw = std::time::Instant::now();
    while !state.should_quit {
        // A noisy tool or streaming source must not starve drawing and input.
        // Remaining events stay queued for the next frame.
        let mut received_agent_event = false;
        for _ in 0..256 {
            let Some(event) = bridge.try_recv_laned() else {
                break;
            };
            handle_agent_envelope(state, storage, event)?;
            received_agent_event = true;
        }
        redraw |= received_agent_event;
        redraw |= state.ready_prune.is_some();
        finish_ready_prune(state, storage)?;
        let now = std::time::Instant::now();
        state.tick(now);
        if redraw || now >= next_periodic_redraw {
            terminal.draw(|frame| render(frame, state, highlighter))?;
            redraw = false;
            next_periodic_redraw = now + std::time::Duration::from_secs(1);
        }
        let poll_timeout = next_periodic_redraw
            .saturating_duration_since(std::time::Instant::now())
            .min(std::time::Duration::from_millis(100));
        if event::poll(poll_timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    let effects = state.handle_key(key);
                    for effect in effects {
                        if let Err(error) =
                            handle_effect(state, storage, paths, bridge, effect.clone())
                        {
                            handle_effect_failure(state, &effect, &error);
                        }
                    }
                    redraw = true;
                }
                Event::Mouse(mouse) => {
                    for effect in mouse_scroll_effects(state, mouse.kind) {
                        if let Err(error) =
                            handle_effect(state, storage, paths, bridge, effect.clone())
                        {
                            handle_effect_failure(state, &effect, &error);
                        }
                    }
                    redraw = true;
                }
                Event::Paste(text) => {
                    let effects = state.handle_paste(&text);
                    for effect in effects {
                        if let Err(error) =
                            handle_effect(state, storage, paths, bridge, effect.clone())
                        {
                            handle_effect_failure(state, &effect, &error);
                        }
                    }
                    redraw = true;
                }
                // Resize/focus events can change terminal geometry or visual
                // state without producing an application effect.
                _ => redraw = true,
            }
        }
    }
    Ok(())
}

pub(crate) fn mouse_scroll_effects(state: &mut AppState, kind: MouseEventKind) -> Vec<Effect> {
    let code = match kind {
        MouseEventKind::ScrollUp => KeyCode::Up,
        MouseEventKind::ScrollDown => KeyCode::Down,
        _ => return Vec::new(),
    };
    let mut effects = Vec::new();
    for _ in 0..3 {
        effects.extend(state.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    effects
}

fn queue_outbound(
    state: &mut AppState,
    bridge: &dyn AgentSink,
    outbound: Outbound,
) -> Result<String> {
    let outbound_id = outbound.id.clone();
    let side_outbound = (state.side_active || state.side_starting)
        && !matches!(&outbound.kind, OutboundKind::Ask { .. });
    let command = if side_outbound {
        AgentCommand::SendSide(outbound)
    } else {
        AgentCommand::Send(outbound)
    };
    bridge.send(command)?;
    state.pending_outbound_ids.insert(outbound_id.clone());
    if side_outbound {
        state.side_outbound_ids.insert(outbound_id.clone());
    }
    state.agent_progress.queue_depth = state.agent_progress.queue_depth.saturating_add(1);
    state.agent_progress.record_queued(
        "Waiting in the Copilot SDK queue",
        format!("Outbound {} has not started yet", short_id(&outbound_id)),
    );
    Ok(outbound_id)
}

fn persist_correction(storage: &Storage, state: &AppState, outbound: &Outbound) -> Result<()> {
    storage.enqueue_chat(&PendingChat {
        id: outbound.id.clone(),
        work_item_id: state.work_item.item.id.clone(),
        text: outbound.text.clone(),
        kind: "correction".into(),
        lane: if state.side_active || state.side_starting {
            "side".into()
        } else {
            "main".into()
        },
        created_at: now(),
    })
}

fn push_outbound_entry(state: &mut AppState, entry: ChatEntry) {
    let side_outbound = entry
        .outbound_id
        .as_ref()
        .is_some_and(|id| state.side_outbound_ids.contains(id));
    if side_outbound && state.side_starting && !state.side_active {
        state.pending_side_entries.push(entry);
    } else if !side_outbound && state.side_active {
        state.main_chat.get_or_insert_with(Vec::new).push(entry);
    } else {
        state.chat.push(entry);
        follow_chat(state);
    }
}

fn mark_outbound_failed_before_delivery(state: &mut AppState, outbound_id: &str, message: String) {
    state.pending_outbound_ids.remove(outbound_id);
    state.side_outbound_ids.remove(outbound_id);
    if let Some(chat) = state
        .chat
        .iter_mut()
        .rev()
        .find(|entry| entry.outbound_id.as_deref() == Some(outbound_id))
    {
        chat.streaming = false;
        chat.error = Some(message);
    }
}

pub(crate) fn handle_effect(
    state: &mut AppState,
    storage: &Storage,
    paths: &AppPaths,
    bridge: &dyn AgentSink,
    effect: Effect,
) -> Result<()> {
    match effect {
        Effect::Quit { force } => {
            if force || (!state.has_unsubmitted_work() && state.compose.is_empty()) {
                state.quit_guard = None;
                state.should_quit = true;
            } else {
                let warning =
                    "QUIT BLOCKED · unsubmitted work exists · :q! force · Esc dismiss".to_owned();
                state.status = warning.clone();
                state.quit_guard = Some(warning);
            }
        }
        Effect::CreateAnnotation {
            kind,
            text,
            selection,
        } => {
            let repo = state
                .work_item
                .repos
                .get(state.repo_index)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no current repository"))?;
            let file = repo
                .diff
                .files
                .get(state.file_index)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no current file"))?;
            let created = create_local_annotation(
                storage,
                &Git::default(),
                AnnotationRequest {
                    repo: &repo.record,
                    current_version: &repo.version,
                    file: &file,
                    selection_start: selection.start_row,
                    selection_end: selection.end_row,
                    kind,
                    text,
                },
            )?;
            let current_placement = Placement {
                version_id: repo.version.id,
                ..created.placement
            };
            let snapshot_note = created
                .snapshot
                .as_ref()
                .map(|snapshot| format!(" · pinned s{}", snapshot.version_num))
                .unwrap_or_default();
            let created_annotation_id = created.annotation.id.clone();
            state
                .annotations
                .push((created.annotation.clone(), current_placement));
            state.status = match kind {
                AnnotationKind::Comment => {
                    format!("Comment saved locally{snapshot_note}")
                }
                AnnotationKind::Ask => {
                    let message = created
                        .ask_message
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("ask message was not created"))?;
                    let outbound = Outbound::new(
                        OutboundKind::Ask {
                            annotation_id: created.annotation.id.clone(),
                            user_message_id: message.id.clone(),
                            assistant_message_id: uuid::Uuid::new_v4().to_string(),
                            assistant_seq: message.seq + 1,
                        },
                        self_contained_ask(&created.annotation, message, &[]),
                    );
                    let outbound_id = outbound.id.clone();
                    state
                        .ask_threads
                        .entry(created.annotation.id.clone())
                        .or_default()
                        .push(message.clone());
                    state.chat.push(ChatEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        role: "you".into(),
                        text: message.text.clone(),
                        streaming: false,
                        annotation_id: Some(created.annotation.id),
                        outbound_id: Some(outbound_id.clone()),
                        error: None,
                    });
                    follow_chat(state);
                    match queue_outbound(state, bridge, outbound) {
                        Ok(_) => {
                            format!("Ask queued for the active Copilot session{snapshot_note}")
                        }
                        Err(error) => {
                            mark_outbound_failed_before_delivery(
                                state,
                                &outbound_id,
                                error.to_string(),
                            );
                            state.pending_asks.push(message.clone());
                            format!(
                                "Ask saved as pending after agent delivery failed{snapshot_note}"
                            )
                        }
                    }
                }
            };
            let stream = state.review_stream();
            if let Some((header, anchor)) =
                stream
                    .rows()
                    .iter()
                    .enumerate()
                    .find_map(|(index, row)| match row {
                        ReviewRow::Annotation { block, .. }
                            if block.annotation_id == created_annotation_id
                                && matches!(block.part, AnnotationRowPart::Header { .. }) =>
                        {
                            Some((index, block.anchor.clone()))
                        }
                        _ => None,
                    })
            {
                state.review_cursor = stream.rows()[..header]
                    .iter()
                    .rposition(|row| {
                        matches!(
                            row,
                            ReviewRow::Source {
                                old_anchor,
                                new_anchor,
                                ..
                            } if old_anchor.as_ref() == Some(&anchor)
                                || new_anchor.as_ref() == Some(&anchor)
                        )
                    })
                    .unwrap_or_else(|| header.saturating_sub(1));
            }
        }
        Effect::SendChat(text) => {
            if !text.trim().is_empty() {
                let outbound = Outbound::new(OutboundKind::Chat, text.clone());
                if !state.side_active && !state.side_starting {
                    storage.enqueue_chat(&PendingChat {
                        id: outbound.id.clone(),
                        work_item_id: state.work_item.item.id.clone(),
                        text: text.clone(),
                        kind: "chat".into(),
                        lane: "main".into(),
                        created_at: now(),
                    })?;
                }
                let outbound_id = queue_outbound(state, bridge, outbound)?;
                let entry = ChatEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: "you".into(),
                    text: text.clone(),
                    streaming: false,
                    annotation_id: None,
                    outbound_id: Some(outbound_id.clone()),
                    error: None,
                };
                push_outbound_entry(state, entry);
                state.status = if state.side_starting {
                    "SIDE message buffered safely while the ephemeral fork starts".into()
                } else if state.status.starts_with("No response is active;") {
                    "No response is active; the correction was queued as a normal prompt".into()
                } else {
                    "Chat message queued".into()
                };
            }
        }
        Effect::SteerChat(text) => {
            if !text.trim().is_empty() {
                let outbound = Outbound::new(OutboundKind::Correction, text.clone());
                let outbound_id = outbound.id.clone();
                persist_correction(storage, state, &outbound)?;
                bridge.send(AgentCommand::Steer(outbound))?;
                state.pending_outbound_ids.insert(outbound_id.clone());
                state.chat.push(ChatEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: "you · steer".into(),
                    text,
                    streaming: false,
                    annotation_id: None,
                    outbound_id: Some(outbound_id),
                    error: None,
                });
                state.agent_progress.record(
                    AgentPhase::Planning,
                    "Steering the active Copilot response",
                    "Immediate delivery requested; if the turn ended concurrently, Copilot may queue it",
                    state.agent_progress.active_outbound_id.clone(),
                );
                follow_chat(state);
                state.status = "Steering sent immediately to the active Copilot response".into();
            }
        }
        Effect::CancelQueued(outbound_id) => {
            bridge.send(AgentCommand::CancelQueued(outbound_id.clone()))?;
            state.status = format!("Cancelling queued prompt {}…", short_id(&outbound_id));
        }
        Effect::ReplaceQueued { outbound_id, text } => {
            let replacement = Outbound::new(OutboundKind::Chat, text.clone());
            let replacement_id = replacement.id.clone();
            bridge.send(AgentCommand::ReplaceQueued {
                outbound_id: outbound_id.clone(),
                replacement,
            })?;
            if let Some(message) = state
                .chat
                .iter_mut()
                .chain(state.pending_side_entries.iter_mut())
                .chain(state.main_chat.iter_mut().flatten())
                .find(|message| message.outbound_id.as_deref() == Some(outbound_id.as_str()))
            {
                message.error = Some("replacement pending".into());
            }
            state.pending_outbound_ids.remove(&outbound_id);
            state.pending_outbound_ids.insert(replacement_id.clone());
            if state.side_outbound_ids.remove(&outbound_id) {
                state.side_outbound_ids.insert(replacement_id.clone());
            }
            let replacement_entry = ChatEntry {
                id: uuid::Uuid::new_v4().to_string(),
                role: "you".into(),
                text,
                streaming: false,
                annotation_id: None,
                outbound_id: Some(replacement_id),
                error: None,
            };
            push_outbound_entry(state, replacement_entry);
            state.status = "Replacing the queued prompt atomically…".into();
        }
        Effect::StartSide(question) => {
            if state.side_active || state.side_starting {
                state.status =
                    "A SIDE conversation is already active or starting · use /main first".into();
            } else {
                state.compose.clear();
                state.compose_cursor = 0;
                state.compose_scroll = 0;
                state.compose_target = Some(ComposeTarget::Chat);
                state.input_mode = InputMode::Normal;
                let outbound = question.filter(|text| !text.trim().is_empty()).map(|text| {
                    let outbound = Outbound::new(OutboundKind::Chat, text.clone());
                    state.pending_side_entries.push(ChatEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        role: "you".into(),
                        text,
                        streaming: false,
                        annotation_id: None,
                        outbound_id: Some(outbound.id.clone()),
                        error: None,
                    });
                    state.pending_outbound_ids.insert(outbound.id.clone());
                    state.side_outbound_ids.insert(outbound.id.clone());
                    state.agent_progress.queue_depth =
                        state.agent_progress.queue_depth.saturating_add(1);
                    outbound
                });
                bridge.send(AgentCommand::StartSide { outbound })?;
                state.side_starting = true;
                follow_chat(state);
                state.agent_progress.record(
                    AgentPhase::Queued,
                    "Creating an ephemeral SIDE fork",
                    "MAIN history remains active and will not receive side messages",
                    None,
                );
                state.status = "Creating SIDE conversation from the current MAIN snapshot".into();
            }
        }
        Effect::ExitSide => {
            if state.side_starting {
                bridge.send(AgentCommand::CancelSide)?;
                restore_main_surface(state);
                state.agent_progress.record(
                    AgentPhase::Stopping,
                    "Cancelling SIDE creation",
                    "MAIN is restored; the SDK fork operation is unwinding in the background",
                    None,
                );
                state.status = "MAIN restored · cancelling SIDE creation in background".into();
            } else if state.side_active {
                bridge.send(AgentCommand::ExitSide)?;
                restore_main_surface(state);
                state.agent_progress.record(
                    AgentPhase::Stopping,
                    "Closing SIDE in the background",
                    "MAIN is already restored; SIDE abort and cleanup are still running",
                    None,
                );
                state.status = "MAIN restored · closing SIDE in background".into();
            } else {
                state.status = "Already on MAIN".into();
            }
        }
        Effect::FollowUpAsk {
            annotation_id,
            text,
        } => {
            if !text.trim().is_empty() {
                let annotation = storage
                    .annotation_by_id(&annotation_id)?
                    .ok_or_else(|| anyhow::anyhow!("ask annotation is missing"))?;
                let thread = storage.ask_messages_for_annotation(&annotation_id)?;
                let message = AskMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    annotation_id: annotation_id.clone(),
                    seq: thread.iter().map(|entry| entry.seq).max().unwrap_or(-1) + 1,
                    role: "user".into(),
                    text: text.clone(),
                    sent: false,
                    delivery_state: DeliveryState::Pending,
                    ts: now(),
                };
                storage.append_ask_message(&message)?;
                if let Some((annotation, _)) = state
                    .annotations
                    .iter_mut()
                    .find(|(annotation, _)| annotation.id == annotation_id)
                {
                    annotation.delivery_state = DeliveryState::Pending;
                }
                state
                    .ask_threads
                    .entry(annotation_id.clone())
                    .or_default()
                    .push(message.clone());
                let outbound = Outbound::new(
                    OutboundKind::Ask {
                        annotation_id: annotation_id.clone(),
                        user_message_id: message.id.clone(),
                        assistant_message_id: uuid::Uuid::new_v4().to_string(),
                        assistant_seq: message.seq + 1,
                    },
                    self_contained_ask(&annotation, &message, &thread),
                );
                let outbound_id = outbound.id.clone();
                state.chat.push(ChatEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: "you".into(),
                    text,
                    streaming: false,
                    annotation_id: Some(annotation_id),
                    outbound_id: Some(outbound_id.clone()),
                    error: None,
                });
                follow_chat(state);
                state.status = match queue_outbound(state, bridge, outbound) {
                    Ok(_) => "Ask follow-up queued".into(),
                    Err(error) => {
                        mark_outbound_failed_before_delivery(
                            state,
                            &outbound_id,
                            error.to_string(),
                        );
                        state.pending_asks.push(message);
                        "Ask follow-up saved as pending after agent delivery failed".into()
                    }
                };
            }
        }
        Effect::EditAnnotation {
            annotation_id,
            text,
        } => {
            let annotation = storage
                .annotation_by_id(&annotation_id)?
                .ok_or_else(|| anyhow::anyhow!("annotation is missing"))?;
            storage.update_annotation_text(&annotation_id, &text)?;
            if let Some((current, _)) = state
                .annotations
                .iter_mut()
                .find(|(current, _)| current.id == annotation_id)
            {
                current.text = Some(text.clone());
            }
            if annotation.submitted {
                let outbound = Outbound::new(
                    OutboundKind::Correction,
                    format!(
                        "Correction to submitted review annotation {}: {}",
                        annotation_id, text
                    ),
                );
                persist_correction(storage, state, &outbound)?;
                queue_outbound(state, bridge, outbound)?;
                state.status = "Annotation updated; correction queued".into();
            } else {
                state.status = "Annotation updated".into();
            }
        }
        Effect::EditAskMessage {
            annotation_id,
            message_id,
            text,
        } => {
            let previous = state
                .ask_threads
                .get(&annotation_id)
                .and_then(|thread| thread.iter().find(|message| message.id == message_id))
                .cloned()
                .context("ask message is missing")?;
            storage.update_ask_message_text(&message_id, &text)?;
            if let Some(message) = state
                .ask_threads
                .get_mut(&annotation_id)
                .and_then(|thread| thread.iter_mut().find(|message| message.id == message_id))
            {
                message.text = text.clone();
            }
            let outbound = Outbound::new(
                OutboundKind::Correction,
                format!(
                    "Correction to ask annotation {annotation_id}, message {message_id}: {text}"
                ),
            );
            persist_correction(storage, state, &outbound)?;
            queue_outbound(state, bridge, outbound)?;
            state.status = if previous.sent {
                "Ask message edited; correction queued".into()
            } else {
                "Queued ask edited; correction will follow it".into()
            };
        }
        Effect::RepinAnnotation {
            annotation_id,
            selection,
        } => {
            let file = state.current_file().cloned().context("no current file")?;
            let repo = state
                .work_item
                .repos
                .get(state.repo_index)
                .context("no current repository")?;
            let anchor = anchor_from_diff(&file, selection.start_row, selection.end_row)?;
            let (annotation, placement) = state
                .annotations
                .iter_mut()
                .find(|(annotation, _)| annotation.id == annotation_id)
                .context("annotation is not visible in this version")?;
            annotation.file_path = file.display_path;
            annotation.anchor_snippet = anchor.snippet;
            annotation.anchor_hash = anchor.hash;
            annotation.anchor_start_offset = anchor.start_offset as i64;
            annotation.anchor_line_count = anchor.selected_count as i64;
            placement.version_id = repo.version.id.clone();
            placement.side = anchor.side;
            placement.line_start = anchor.line_start as i64;
            placement.line_end = anchor.line_end as i64;
            placement.outdated = false;
            placement.ambiguous = false;
            storage.update_annotation_anchor(annotation)?;
            storage.upsert_placement(placement)?;
            state.status = "Annotation anchor re-pinned".into();
        }
        Effect::DeleteAnnotation(annotation_id) => {
            let mut pending_ids = state
                .chat
                .iter()
                .chain(state.pending_side_entries.iter())
                .chain(state.main_chat.iter().flatten())
                .filter(|message| message.annotation_id.as_deref() == Some(annotation_id.as_str()))
                .filter_map(|message| message.outbound_id.clone())
                .filter(|outbound_id| state.pending_outbound_ids.contains(outbound_id))
                .collect::<Vec<_>>();
            pending_ids.sort();
            pending_ids.dedup();
            for outbound_id in &pending_ids {
                let command =
                    if state.agent_progress.active_outbound_id.as_ref() == Some(outbound_id) {
                        AgentCommand::Abort
                    } else {
                        AgentCommand::CancelQueued(outbound_id.clone())
                    };
                bridge.send(command)?;
            }
            let annotation = storage
                .annotation_by_id(&annotation_id)?
                .ok_or_else(|| anyhow::anyhow!("annotation is missing"))?;
            let placements = storage.placements_for_annotation(&annotation_id)?;
            let ask_messages = if annotation.kind == AnnotationKind::Ask {
                storage.ask_messages_for_annotation(&annotation_id)?
            } else {
                Vec::new()
            };
            if annotation.submitted || annotation.delivery_state == DeliveryState::Sent {
                let outbound = Outbound::new(
                    OutboundKind::Correction,
                    format!("Correction: review annotation {annotation_id} was deleted."),
                );
                persist_correction(storage, state, &outbound)?;
                queue_outbound(state, bridge, outbound)?;
            }
            storage.delete_annotation(&annotation_id)?;
            state
                .annotations
                .retain(|(current, _)| current.id != annotation_id);
            state.ask_threads.remove(&annotation_id);
            state.deleted_annotation = Some((annotation, placements, ask_messages));
            let review_rows = state.review_stream().rows().len();
            state.review_cursor = state.review_cursor.min(review_rows.saturating_sub(1));
            state.review_scroll = state.review_scroll.min(review_rows.saturating_sub(1));
            state.status = "Annotation deleted · u undo".into();
        }
        Effect::UndoAnnotation => {
            if let Some((annotation, placements, ask_messages)) = state.deleted_annotation.take() {
                if let Some(first) = placements.first() {
                    storage.add_annotation(&annotation, first)?;
                    for placement in placements.iter().skip(1) {
                        storage.upsert_placement(placement)?;
                    }
                    for message in &ask_messages {
                        storage.append_ask_message(message)?;
                    }
                    if let Some(current) = placements.iter().find(|placement| {
                        state
                            .work_item
                            .repos
                            .iter()
                            .any(|repo| repo.version.id == placement.version_id)
                    }) {
                        state
                            .annotations
                            .push((annotation.clone(), current.clone()));
                    }
                    if annotation.kind == AnnotationKind::Ask {
                        state
                            .ask_threads
                            .insert(annotation.id.clone(), ask_messages);
                    }
                    state.status = "Annotation restored".into();
                }
            } else {
                state.status = "Nothing to undo".into();
            }
        }
        Effect::OpenVersion {
            repo_id,
            version_id,
        } => {
            open_version(state, storage, paths, &repo_id, &version_id)?;
            state.screen = Screen::Review;
            state.status = format!("Opened version {version_id}");
        }
        Effect::LoadPrune => {
            state.prune_items = storage
                .review_history()?
                .into_iter()
                .map(|item| PruneChoice {
                    id: item.id,
                    name: item.name,
                    last_opened_at: item.last_opened_at,
                    versions: item.versions,
                    annotations: item.annotations,
                    selected: false,
                })
                .collect();
            state.prune_index = 0;
            state.status = format!("{} reviewed Work Items", state.prune_items.len());
        }
        Effect::PruneWorkItems { ids, export_first } => {
            if state.pending_prune.is_some() {
                state.status =
                    "A prune is already deleting Copilot sessions; wait for its result".into();
                return Ok(());
            }
            let current_id = &state.work_item.item.id;
            let skipped_current = ids.iter().filter(|id| *id == current_id).count();
            let eligible = ids
                .into_iter()
                .filter(|id| id != current_id)
                .collect::<Vec<_>>();
            if eligible.is_empty() {
                state.status =
                    "The open Work Item cannot be pruned; switch to another Work Item first".into();
                return Ok(());
            }
            let request_id = uuid::Uuid::new_v4().to_string();
            bridge.send(AgentCommand::PruneSessions {
                request_id: request_id.clone(),
                work_item_ids: eligible.clone(),
                export_first,
                paths: paths.clone(),
            })?;
            state.pending_prune = Some(PendingPrune {
                request_id,
                work_item_ids: eligible.clone(),
                skipped_current,
            });
            state.agent_progress.record(
                AgentPhase::Queued,
                "Prune queued",
                format!(
                    "{} Work Item(s) waiting for the current Copilot turn to finish",
                    eligible.len()
                ),
                None,
            );
            state.status = if skipped_current == 0 {
                format!(
                    "Prune queued for {} Work Item(s)… waiting for the current Copilot turn; local history is unchanged",
                    eligible.len()
                )
            } else {
                format!(
                    "Prune queued for {} Work Item(s)… waiting for the current Copilot turn; skipped the open Work Item",
                    eligible.len()
                )
            };
        }
        Effect::ResendPendingAsk(message) => {
            let annotation = storage
                .annotation_by_id(&message.annotation_id)?
                .ok_or_else(|| anyhow::anyhow!("pending ask annotation is missing"))?;
            let thread = storage.ask_messages_for_annotation(&annotation.id)?;
            let tail = thread
                .iter()
                .filter(|entry| entry.id != message.id)
                .cloned()
                .collect::<Vec<_>>();
            let outbound_id = queue_outbound(
                state,
                bridge,
                Outbound::new(
                    OutboundKind::Ask {
                        annotation_id: annotation.id.clone(),
                        user_message_id: message.id.clone(),
                        assistant_message_id: uuid::Uuid::new_v4().to_string(),
                        assistant_seq: thread.iter().map(|entry| entry.seq).max().unwrap_or(0) + 1,
                    },
                    self_contained_ask(&annotation, &message, &tail),
                ),
            )?;
            push_outbound_entry(
                state,
                ChatEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: "you (resent)".into(),
                    text: message.text.clone(),
                    streaming: false,
                    annotation_id: Some(annotation.id),
                    outbound_id: Some(outbound_id.clone()),
                    error: None,
                },
            );
            finish_recovery_item(state, &message.id);
            state.status = "Pending ask intentionally resent".into();
        }
        Effect::ResendPendingChat(chat) => {
            let outbound = Outbound {
                id: chat.id.clone(),
                kind: if chat.kind == "correction" {
                    OutboundKind::Correction
                } else {
                    OutboundKind::Chat
                },
                text: chat.text.clone(),
            };
            let outbound_id = queue_outbound(state, bridge, outbound)?;
            push_outbound_entry(
                state,
                ChatEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: if chat.kind == "correction" {
                        "you · correction (resent)".into()
                    } else {
                        "you (resent)".into()
                    },
                    text: chat.text,
                    streaming: false,
                    annotation_id: None,
                    outbound_id: Some(outbound_id),
                    error: None,
                },
            );
            state.pending_chats.retain(|pending| pending.id != chat.id);
            finish_recovery(state);
            state.status = if chat.kind == "correction" {
                format!(
                    "Pending {} correction intentionally resent",
                    chat.lane.to_uppercase()
                )
            } else {
                "Pending Chat prompt intentionally resent".into()
            };
        }
        Effect::DiscardPendingChat(id) => {
            storage.delete_queued_chat(&id)?;
            state.pending_chats.retain(|pending| pending.id != id);
            finish_recovery(state);
            state.status = "Pending Chat prompt discarded without resending".into();
        }
        Effect::DiscardPendingAsk(message_id) => {
            storage.discard_pending_ask(&message_id)?;
            finish_recovery_item(state, &message_id);
            state.status = "Pending ask discarded without resending".into();
        }
        Effect::ResendPendingComments => {
            let mut export = CommentExport::load(storage, &state.work_item.item)?;
            export
                .comments
                .retain(|comment| state.pending_comment_ids.contains(&comment.annotation_id));
            let outbound_id = queue_outbound(
                state,
                bridge,
                Outbound::new(
                    OutboundKind::CommentBatch {
                        annotation_ids: state.pending_comment_ids.clone(),
                    },
                    export.structured_session_message(),
                ),
            )?;
            push_outbound_entry(
                state,
                ChatEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: "comments (resent)".into(),
                    text: export.structured_session_message(),
                    streaming: false,
                    annotation_id: None,
                    outbound_id: Some(outbound_id.clone()),
                    error: None,
                },
            );
            state.pending_comment_ids.clear();
            finish_recovery(state);
            state.status = "Pending comment batch intentionally resent".into();
        }
        Effect::DiscardPendingComments => {
            storage.mark_comments_delivery(&state.pending_comment_ids, DeliveryState::Draft)?;
            state.pending_comment_ids.clear();
            finish_recovery(state);
            state.status = "Pending comment batch discarded without resending".into();
        }
        Effect::ResendPendingContext => {
            let context = storage
                .context_for_work_item(&state.work_item.item.id)?
                .context("pending context is missing")?;
            queue_outbound(
                state,
                bridge,
                Outbound::new(
                    OutboundKind::Context {
                        work_item_id: state.work_item.item.id.clone(),
                    },
                    format!(
                        "Accepted review context for this Work Item:\n{}",
                        render_review_context(&context)
                    ),
                ),
            )?;
            state.pending_context = false;
            finish_recovery(state);
            state.status = "Pending context intentionally resent".into();
        }
        Effect::DiscardPendingContext => {
            storage.mark_context_delivery(&state.work_item.item.id, DeliveryState::Draft)?;
            state.pending_context = false;
            finish_recovery(state);
            state.status = "Pending context discarded without resending".into();
        }
        Effect::AbortAgent => {
            if state.agent_progress.phase != AgentPhase::Stopping {
                state.agent_progress.record(
                    AgentPhase::Stopping,
                    "Stopping the active Copilot response",
                    "Cancellation was requested; waiting for the SDK idle event",
                    state.agent_progress.active_outbound_id.clone(),
                );
            }
            bridge.send(AgentCommand::Abort)?;
            state.status = "Stopping the current Copilot response…".into();
        }
        Effect::Fork => {
            bridge.send(AgentCommand::Fork)?;
            state.status = "Forking active session…".into();
        }
        Effect::Compact(instructions) => {
            bridge.send(AgentCommand::Compact(instructions))?;
            state.status = "Compacting active session…".into();
        }
        Effect::LoadModels => {
            bridge.send(AgentCommand::ListModels)?;
            state.status = "Loading model capabilities from the Copilot runtime…".into();
        }
        Effect::SelectModel(selection) => {
            bridge.send(AgentCommand::SelectModel(selection.clone()))?;
            state.status = format!(
                "Switching to {} · reasoning {} · context {}…",
                selection.model_id,
                selection
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("runtime default"),
                selection
                    .context_tier
                    .as_deref()
                    .unwrap_or("runtime default"),
            );
        }
        Effect::SetModel(model) => {
            persist_model_preferences(
                state,
                storage,
                &crate::copilot::ModelSelection {
                    model_id: model.clone(),
                    reasoning_effort: None,
                    context_tier: None,
                },
            )?;
            bridge.send(AgentCommand::SetModel(model.clone()))?;
            state.model = model.clone();
            state.reasoning_effort = None;
            state.context_tier = None;
            state.status = format!("Switching model to {model}…");
        }
        Effect::Snapshot => {
            let repo = state
                .work_item
                .repos
                .get(state.repo_index)
                .ok_or_else(|| anyhow::anyhow!("no current repository"))?;
            if repo.version.kind != crate::domain::VersionKind::WorkingTree {
                state.status = "Snapshots are only available for local working trees".into();
            } else {
                let snapshot = create_snapshot(storage, &Git::default(), &repo.record)?;
                state.status = format!("Pinned snapshot s{}", snapshot.version_num);
            }
        }
        Effect::ExpandContext { all } => {
            expand_context(state, storage, all)?;
            state.status = if all {
                "Expanded full context for the current file".into()
            } else {
                "Expanded diff context".into()
            };
        }
        Effect::Export { format } => {
            for repo in &state.work_item.repos {
                if repo.version.kind == crate::domain::VersionKind::WorkingTree {
                    create_snapshot(storage, &Git::default(), &repo.record)?;
                }
            }
            let export = CommentExport::load(storage, &state.work_item.item)?;
            if export.comments.is_empty() {
                state.status = "No pending comments to export".into();
            } else {
                let mut export_path = None;
                if let Some(requested) = format {
                    let format = ExportFormat::parse(&requested)?;
                    let path = paths.exports.join(format!(
                        "{}-{}.{}",
                        state.work_item.item.name.replace(['/', ' '], "-"),
                        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
                        format.extension()
                    ));
                    export.write(format, &path)?;
                    export_path = Some(path);
                }
                let annotation_ids = export
                    .comments
                    .iter()
                    .map(|comment| comment.annotation_id.clone())
                    .collect::<Vec<_>>();
                storage.mark_comments_delivery(&annotation_ids, DeliveryState::Pending)?;
                state.pending_comment_ids = annotation_ids.clone();
                let outbound_id = queue_outbound(
                    state,
                    bridge,
                    Outbound::new(
                        OutboundKind::CommentBatch { annotation_ids },
                        export.structured_session_message(),
                    ),
                )?;
                push_outbound_entry(
                    state,
                    ChatEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        role: "comments".into(),
                        text: export.structured_session_message(),
                        streaming: false,
                        annotation_id: None,
                        outbound_id: Some(outbound_id),
                        error: None,
                    },
                );
                state.screen = Screen::Chat;
                state.focus = Focus::Chat;
                state.input_mode = InputMode::Normal;
                state.chat_autofollow = true;
                state.chat_cursor = state.chat.len().saturating_sub(1);
                state.chat_scroll = state
                    .chat_total_rows
                    .saturating_sub(state.chat_viewport_rows);
                state.status = export_path.map_or_else(
                    || "Comment batch queued".into(),
                    |path| format!("Exported comments to {} · batch queued", path.display()),
                );
            }
        }
        Effect::GenerateContext => {
            let changed_files = state
                .work_item
                .repos
                .iter()
                .flat_map(|repo| {
                    repo.diff.files.iter().map(move |file| {
                        format!("{}/{}", repo.record.name, file.display_path.display())
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            let prompt = format!(
                "Draft structured review context for this local code change. \
                 Inspect the repositories read-only as needed. Return exactly these six \
                 one-line fields, with these labels:\n\
                 Title:\nWhat:\nWhy:\nHow:\nConsiderations:\nOther approaches:\n\
                 Changed files:\n{changed_files}"
            );
            state.context_draft.clear();
            state.context_streaming = true;
            queue_outbound(
                state,
                bridge,
                Outbound::new(OutboundKind::ContextDraft, prompt),
            )?;
            state.status = "Generating structured context…".into();
        }
        Effect::AttachContext(draft) => {
            let mut context = parse_review_context(&state.work_item.item.id, &draft);
            context.delivery_state = DeliveryState::Pending;
            storage.upsert_context(&context)?;
            state.pending_context = true;
            let context_dir = state.work_item.session_root.join(".rq-tui");
            std::fs::create_dir_all(&context_dir)?;
            std::fs::write(
                context_dir.join("context.md"),
                format!(
                    "# Work Item context\n\n{}\n",
                    render_review_context(&context)
                ),
            )?;
            queue_outbound(
                state,
                bridge,
                Outbound::new(
                    OutboundKind::Context {
                        work_item_id: state.work_item.item.id.clone(),
                    },
                    format!(
                        "Accepted review context for this Work Item:\n{}",
                        render_review_context(&context)
                    ),
                ),
            )?;
            state.screen = state.previous_screen;
            state.status = "Context attached and queued for the active session".into();
        }
        Effect::Sync => {
            refresh_work_item(state, storage, paths)?;
            if !prompt_unseen_remote_version(state) {
                state.status = "Repositories synchronized".into();
            }
        }
        Effect::SetBase { branch, repo } => {
            let target = match repo {
                Some(name) => state
                    .work_item
                    .repos
                    .iter_mut()
                    .find(|candidate| candidate.record.name == name)
                    .ok_or_else(|| anyhow::anyhow!("unknown repository: {name}"))?,
                None => state
                    .work_item
                    .repos
                    .get_mut(state.repo_index)
                    .ok_or_else(|| anyhow::anyhow!("no current repository"))?,
            };
            if target.record.remote_pr_url.is_some() {
                anyhow::bail!("base overrides only apply to local repositories");
            }
            target.record.base_branch = Some(branch.clone());
            target.record.base_branch_source = BaseBranchSource::PerRepo;
            storage.upsert_repo(&target.record)?;
            refresh_work_item(state, storage, paths)?;
            state.status = format!("Base changed to {branch}");
        }
        Effect::SetExpandStep(step) => {
            storage.set_setting("diff.expand_step", &step.to_string())?;
            state.expand_step = step;
            state.status = format!("Diff expansion step changed to {step}");
        }
        Effect::SetDefaultDiffLayout(layout) => {
            storage.set_setting("diff.layout.default", layout.label())?;
            state.default_layout = layout;
            state.status = format!(
                "Default diff layout: {} · current review unchanged",
                layout.label()
            );
        }
        Effect::SetFileTreeDefault(open) => {
            storage.set_setting("file_tree.default", if open { "open" } else { "closed" })?;
            state.file_tree_default_open = open;
            state.status = format!(
                "File tree will start {} · press t to change the current review",
                if open { "open" } else { "closed" }
            );
        }
        Effect::SetMarkdownPreview(preview) => {
            storage.set_setting("markdown.preview", preview.label())?;
            state.markdown_preview = preview;
            state.status = format!(
                "Markdown preview: {} · gm uses this default",
                preview.label()
            );
        }
        Effect::Preview => match markdown_preview_source(state) {
            Ok(markdown) => state.open_preview(markdown),
            Err(error) => state.status = error.to_string(),
        },
        Effect::PreviewBrowser => {
            open_browser_preview(state, paths, platform_preview_opener());
        }
        Effect::Yank(text) => {
            state.status = match copy_to_clipboard(&text)? {
                ClipboardDelivery::Native(program) => {
                    format!("Copied {} bytes with {program}", text.len())
                }
                ClipboardDelivery::Osc52 => format!(
                    "Sent {} bytes via OSC 52; terminal confirmation unavailable",
                    text.len()
                ),
            };
        }
    }
    Ok(())
}

pub(crate) fn handle_effect_failure(state: &mut AppState, effect: &Effect, error: &anyhow::Error) {
    state.status = format!("Action failed: {error:#}");
    match effect {
        Effect::CreateAnnotation {
            kind,
            text,
            selection,
        } => {
            state.cursor = selection.end_row;
            state.visual_anchor =
                (selection.start_row != selection.end_row).then_some(selection.start_row);
            state.input_return_mode = if state.visual_anchor.is_some() {
                InputMode::Visual
            } else {
                InputMode::Normal
            };
            state.input_mode = InputMode::Compose;
            state.compose_target = Some(ComposeTarget::Annotation(*kind));
            state.compose = text.clone();
            state.compose_cursor = state.compose.len();
        }
        Effect::FollowUpAsk {
            annotation_id,
            text,
        } => {
            state.input_return_mode = InputMode::Normal;
            state.input_mode = InputMode::Compose;
            state.compose_target = Some(ComposeTarget::FollowUp(annotation_id.clone()));
            state.compose = text.clone();
            state.compose_cursor = state.compose.len();
        }
        Effect::EditAnnotation {
            annotation_id,
            text,
        } => {
            state.input_return_mode = InputMode::Normal;
            state.input_mode = InputMode::Compose;
            state.compose_target = Some(ComposeTarget::EditAnnotation(annotation_id.clone()));
            state.compose = text.clone();
            state.compose_cursor = state.compose.len();
        }
        Effect::SendChat(text) => {
            state.screen = Screen::Chat;
            state.input_return_mode = InputMode::Normal;
            state.input_mode = InputMode::Compose;
            state.compose_target = Some(ComposeTarget::Chat);
            state.compose = text.clone();
            state.compose_cursor = state.compose.len();
        }
        Effect::SteerChat(text) => {
            state.screen = Screen::Chat;
            state.input_return_mode = InputMode::Normal;
            state.input_mode = InputMode::Compose;
            state.compose_target = Some(ComposeTarget::Chat);
            state.compose = format!("/steer {text}");
            state.compose_cursor = state.compose.len();
            state.status = format!("Could not steer the active response: {error:#}");
        }
        Effect::AbortAgent => {
            state.agent_progress.record(
                AgentPhase::Responding,
                "Could not request cancellation",
                format!(
                    "{error:#} · the response may still be active; inspect :agent-status and retry"
                ),
                state.agent_progress.active_outbound_id.clone(),
            );
            state.status =
                format!("Could not stop the Copilot response: {error:#} · retry with :stop");
        }
        Effect::CancelQueued(outbound_id) => {
            state.status = format!(
                "Could not cancel queued prompt {}: {error:#}",
                short_id(outbound_id)
            );
        }
        Effect::ReplaceQueued { outbound_id, text } => {
            state.screen = Screen::Chat;
            state.input_return_mode = InputMode::Normal;
            state.input_mode = InputMode::Compose;
            state.compose_target = Some(ComposeTarget::EditQueued(outbound_id.clone()));
            state.compose = text.clone();
            state.compose_cursor = state.compose.len();
            state.status = format!("Could not replace the queued prompt: {error:#}");
        }
        Effect::StartSide(question) => {
            for entry in state.pending_side_entries.drain(..) {
                if let Some(id) = entry.outbound_id {
                    state.pending_outbound_ids.remove(&id);
                    state.side_outbound_ids.remove(&id);
                }
            }
            state.side_starting = false;
            // Consume the MAIN viewport snapshot captured before StartSide.
            // Otherwise a failed fork can restore stale navigation during an
            // unrelated later semantic-layout reset.
            state.reset_chat_semantics();
            state.screen = Screen::Chat;
            state.input_return_mode = InputMode::Normal;
            state.input_mode = InputMode::Compose;
            state.compose_target = Some(ComposeTarget::Chat);
            state.compose = question
                .as_ref()
                .map(|question| format!("/side {question}"))
                .unwrap_or_else(|| "/side".into());
            state.compose_cursor = state.compose.len();
        }
        Effect::ExitSide => {
            state.agent_progress.record(
                AgentPhase::Failed,
                "Could not leave SIDE",
                format!("{error:#}"),
                None,
            );
        }
        Effect::LoadModels => {
            state.screen = state.previous_screen;
            state.status = format!("Could not load Copilot models: {error:#}");
        }
        Effect::SelectModel(selection) => {
            state.status = format!(
                "Could not apply model selection for {}: {error:#}",
                selection.model_id
            );
        }
        _ => {}
    }
}

fn open_version(
    state: &mut AppState,
    storage: &Storage,
    paths: &AppPaths,
    repo_id: &str,
    version_id: &str,
) -> Result<()> {
    let version = storage
        .versions_for_repo(repo_id)?
        .into_iter()
        .find(|version| version.id == version_id)
        .context("version no longer exists")?;
    if version.kind == crate::domain::VersionKind::WorkingTree {
        refresh_work_item(state, storage, paths)?;
        return Ok(());
    }
    let repo_index = state
        .work_item
        .repos
        .iter()
        .position(|repo| repo.record.id == repo_id)
        .context("version repository is not in this Work Item")?;
    let repo = state.work_item.repos[repo_index].record.clone();
    let git = Git::default();
    let (base, target) = match version.kind {
        crate::domain::VersionKind::Snapshot => {
            let working_tree = storage
                .versions_for_repo(repo_id)?
                .into_iter()
                .find(|candidate| candidate.kind == crate::domain::VersionKind::WorkingTree)
                .context("snapshot repository has no v0 base")?;
            let target = paths
                .cache
                .join("history")
                .join(repo_id)
                .join(format!("s{}", version.version_num));
            git.materialize_worktree(&repo.path, &version.head_sha, &target)?;
            (working_tree.head_sha, target)
        }
        crate::domain::VersionKind::Remote => {
            let reference = PrReference::parse(
                repo.remote_pr_url
                    .as_deref()
                    .context("remote version has no PR URL")?,
            )?;
            let base_ref = format!("refs/rq-tui/pr/{}/base", reference.number);
            let merge_base = git.merge_base(&repo.path, &base_ref, &version.head_sha)?;
            (
                merge_base,
                version
                    .worktree_path
                    .clone()
                    .context("remote version has no checkout")?,
            )
        }
        crate::domain::VersionKind::WorkingTree => unreachable!(),
    };
    let raw = git.diff_commits(&repo.path, &base, &version.head_sha, 6)?;
    state.work_item.repos[repo_index].version = version.clone();
    state.work_item.repos[repo_index].diff = crate::diff::parse_unified(&raw)?;
    set_session_link(&state.work_item.session_root.join(&repo.name), &target)?;
    state.repo_index = repo_index;
    state.file_index = 0;
    state.cursor = 0;
    state.scroll = 0;
    state
        .annotations
        .retain(|(annotation, _)| annotation.repo_id != repo_id);
    state
        .annotations
        .extend(storage.annotations_for_version(version_id)?);
    state.ask_threads.retain(|annotation_id, _| {
        state
            .annotations
            .iter()
            .any(|(annotation, _)| &annotation.id == annotation_id)
    });
    for (annotation, _) in state.annotations.iter().filter(|(annotation, _)| {
        annotation.repo_id == repo_id && annotation.kind == AnnotationKind::Ask
    }) {
        state.ask_threads.insert(
            annotation.id.clone(),
            storage.ask_messages_for_annotation(&annotation.id)?,
        );
    }
    state.review_scroll = 0;
    state.sync_review_cursor_to_current_file();
    storage.mark_version_opened(version_id)?;
    if version.kind == crate::domain::VersionKind::Remote {
        for stale in storage.unopened_remote_versions_older_than(repo_id, version.version_num)? {
            if let Some(worktree) = stale.worktree_path {
                git.remove_worktree(&repo.path, &worktree)?;
            }
            storage.delete_version(&stale.id)?;
        }
        load_version_choices(state, storage)?;
    }
    Ok(())
}

fn expand_context(state: &mut AppState, storage: &Storage, all: bool) -> Result<()> {
    let repo = state
        .work_item
        .repos
        .get(state.repo_index)
        .cloned()
        .context("no current repository")?;
    let current_path = repo
        .diff
        .files
        .get(state.file_index)
        .map(|file| file.display_path.clone())
        .context("no current changed file")?;
    let context = state.next_context_lines(all);
    let git = Git::default();
    let raw = match repo.version.kind {
        crate::domain::VersionKind::WorkingTree => {
            git.diff(&repo.record.path, &repo.version.head_sha, context)?
        }
        crate::domain::VersionKind::Snapshot => {
            let base = storage
                .versions_for_repo(&repo.record.id)?
                .into_iter()
                .find(|version| version.kind == crate::domain::VersionKind::WorkingTree)
                .context("snapshot repository has no working-tree base")?;
            git.diff_commits(
                &repo.record.path,
                &base.head_sha,
                &repo.version.head_sha,
                context,
            )?
        }
        crate::domain::VersionKind::Remote => {
            let reference = PrReference::parse(
                repo.record
                    .remote_pr_url
                    .as_deref()
                    .context("remote repository has no PR URL")?,
            )?;
            let base_ref = format!("refs/rq-tui/pr/{}/base", reference.number);
            let base = git.merge_base(&repo.record.path, &base_ref, &repo.version.head_sha)?;
            git.diff_commits(&repo.record.path, &base, &repo.version.head_sha, context)?
        }
    };
    let expanded = crate::diff::parse_unified(&raw)?
        .files
        .into_iter()
        .find(|file| file.display_path == current_path)
        .context("current file disappeared from expanded diff")?;
    state.work_item.repos[state.repo_index].diff.files[state.file_index] = expanded;
    state.cursor = state
        .cursor
        .min(state.current_line_count().saturating_sub(1));
    Ok(())
}

fn set_session_link(link: &std::path::Path, target: &std::path::Path) -> Result<()> {
    if link.symlink_metadata().is_ok() {
        if std::fs::read_link(link).is_ok_and(|existing| existing == target) {
            return Ok(());
        }
        std::fs::remove_file(link)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

pub(crate) fn finish_ready_prune(state: &mut AppState, storage: &Storage) -> Result<()> {
    let Some(ready) = state.ready_prune.take() else {
        return Ok(());
    };
    let Some(pending) = state.pending_prune.take() else {
        return Ok(());
    };
    if ready.request_id != pending.request_id {
        state.pending_prune = Some(pending);
        return Ok(());
    }

    let mut pruned = Vec::new();
    let mut retained_failures = Vec::new();
    let mut remote_failures = Vec::new();
    let mut warnings = Vec::new();
    for work_item_id in &pending.work_item_ids {
        let outcome = ready
            .outcomes
            .iter()
            .find(|outcome| outcome.work_item_id == *work_item_id);
        if storage.work_item_by_id(work_item_id)?.is_none() {
            pruned.push(work_item_id.clone());
            match outcome {
                Some(outcome) if outcome.remote_deleted => {
                    if let Some(error) = outcome.error.as_deref() {
                        warnings.push(format!("{work_item_id}: {error}"));
                    }
                }
                Some(outcome) => remote_failures.push(format!(
                    "{}: {}",
                    work_item_id,
                    outcome
                        .error
                        .as_deref()
                        .unwrap_or("local history is gone, but remote cleanup is unresolved")
                )),
                None => remote_failures.push(format!(
                    "{work_item_id}: local history is gone, but the worker returned no result"
                )),
            }
            continue;
        }
        match outcome {
            Some(outcome) if outcome.local_deleted => {
                retained_failures.push(format!(
                    "{work_item_id}: cleanup reported success, but local history still exists"
                ));
            }
            Some(outcome) => retained_failures.push(format!(
                "{}: {}",
                work_item_id,
                outcome.error.as_deref().unwrap_or("session cleanup failed")
            )),
            None => retained_failures.push(format!(
                "{work_item_id}: Copilot worker returned no cleanup result; local history was retained"
            )),
        }
    }

    state
        .prune_items
        .retain(|item| !pruned.iter().any(|id| id == &item.id));
    state.prune_index = state
        .prune_index
        .min(state.prune_items.len().saturating_sub(1));
    let retained = retained_failures.len();
    let unresolved_remote = remote_failures.len();
    let skipped = pending.skipped_current;
    if retained == 0 && unresolved_remote == 0 && warnings.is_empty() {
        state.agent_progress.record(
            AgentPhase::Idle,
            "Prune complete",
            format!(
                "Deleted Copilot sessions and local history for {} Work Item(s)",
                pruned.len()
            ),
            None,
        );
        state.status = if skipped == 0 {
            format!(
                "Pruned {} Work Item(s) · Copilot sessions and local history deleted",
                pruned.len()
            )
        } else {
            format!(
                "Pruned {} Work Item(s) · skipped the open Work Item",
                pruned.len()
            )
        };
    } else if retained == 0 && unresolved_remote == 0 {
        let detail = warnings.join(" · ");
        state.agent_progress.record(
            AgentPhase::Idle,
            "Prune completed with a journal warning",
            detail.clone(),
            None,
        );
        state.status = format!(
            "Pruned {} Work Item(s) · local history deleted · {}",
            pruned.len(),
            detail
        );
    } else {
        let mut details = Vec::new();
        details.extend(remote_failures);
        details.extend(retained_failures);
        details.extend(warnings);
        let detail = details.join(" · ");
        state
            .agent_progress
            .record(AgentPhase::Failed, "Prune incomplete", detail.clone(), None);
        state.status = format!(
            "Local history deleted {} · remote unresolved {} · retained local history {}{} · {}",
            pruned.len(),
            unresolved_remote,
            retained,
            if skipped == 0 {
                String::new()
            } else {
                format!(" · skipped open {skipped}")
            },
            detail
        );
    }
    Ok(())
}

#[derive(Debug)]
enum ClipboardDelivery {
    Native(&'static str),
    Osc52,
}

fn copy_to_clipboard(text: &str) -> Result<ClipboardDelivery> {
    let forced_osc52 = std::env::var_os("RQ_TUI_CLIPBOARD")
        .is_some_and(|value| value == std::ffi::OsStr::new("osc52"));
    let mut stdout = io::stdout();
    copy_to_clipboard_with_writer(text, forced_osc52, &mut stdout)
}

fn copy_to_clipboard_with_writer(
    text: &str,
    forced_osc52: bool,
    output: &mut dyn std::io::Write,
) -> Result<ClipboardDelivery> {
    if !forced_osc52 {
        if let Ok(program) = copy_with_native_clipboard(text) {
            return Ok(ClipboardDelivery::Native(program));
        }
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let sequence = format!("\u{1b}]52;c;{encoded}\u{7}");
    output.write_all(sequence.as_bytes())?;
    output.flush()?;
    Ok(ClipboardDelivery::Osc52)
}

fn copy_with_native_clipboard(text: &str) -> Result<&'static str> {
    const CLIPBOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
    let candidates: &[(&'static str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    for (program, args) in candidates {
        if run_clipboard_candidate(program, args, text, CLIPBOARD_TIMEOUT) {
            return Ok(program);
        }
    }
    anyhow::bail!("no native clipboard backend accepted the text")
}

fn run_clipboard_candidate(
    program: &str,
    args: &[&str],
    text: &str,
    timeout: std::time::Duration,
) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let payload = text.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&payload).is_ok());
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return status.success() && writer.join().unwrap_or(false);
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = writer.join();
                return false;
            }
        }
    }
}

fn markdown_preview_source(state: &AppState) -> Result<String> {
    let markdown = if state.compose_target.is_some() && !state.compose.is_empty() {
        state.compose.clone()
    } else if state.screen == Screen::Chat {
        state
            .chat
            .get(state.chat_cursor)
            .map(|message| message.text.clone())
            .context("no chat message under cursor")?
    } else if let Some((annotation, _)) =
        state.annotations.iter().find(|(annotation, placement)| {
            let Some(repo) = state.work_item.repos.get(state.repo_index) else {
                return false;
            };
            let Some(file) = state.current_file() else {
                return false;
            };
            annotation.repo_id == repo.record.id
                && annotation.file_path == file.display_path
                && file.visible_lines().nth(state.cursor).is_some_and(|line| {
                    line.new_line.or(line.old_line).map(|number| number as i64)
                        == Some(placement.line_start)
                })
        })
    {
        annotation
            .text
            .clone()
            .unwrap_or_else(|| annotation.anchor_snippet.clone())
    } else {
        let file = state.current_file().context("no file under cursor")?;
        let path = file.path();
        let extension = path.extension().and_then(|value| value.to_str());
        if !matches!(extension, Some("md" | "markdown" | "mdx")) {
            anyhow::bail!("Markdown preview applies to Markdown files or messages");
        }
        let repo = state
            .work_item
            .repos
            .get(state.repo_index)
            .context("no repository under cursor")?;
        std::fs::read_to_string(repo.record.path.join(path))?
    };
    Ok(markdown)
}

fn open_browser_preview(state: &mut AppState, paths: &AppPaths, opener: &str) {
    let source = if state.screen == Screen::Preview {
        state
            .preview_markdown
            .clone()
            .context("inline Markdown preview is empty")
    } else {
        markdown_preview_source(state)
    };
    let markdown = match source {
        Ok(markdown) => markdown,
        Err(error) => {
            state.status = error.to_string();
            return;
        }
    };
    if state.screen != Screen::Preview {
        state.open_preview(markdown.clone());
    }
    match write_markdown_preview(&markdown, &state.work_item.item.id, paths) {
        Ok(path) => match Command::new(opener).arg(&path).spawn() {
            Ok(_) => {
                state.status = format!(
                    "Browser preview requested with {opener} · inline fallback remains open"
                );
            }
            Err(error) => {
                state.status =
                    format!("Browser preview unavailable: {error:#} · inline preview remains open");
            }
        },
        Err(error) => {
            state.status =
                format!("Cannot write browser preview: {error:#} · inline preview remains open");
        }
    }
}

fn write_markdown_preview(
    markdown: &str,
    work_item_id: &str,
    paths: &AppPaths,
) -> Result<std::path::PathBuf> {
    let preview_dir = paths.cache.join("previews");
    std::fs::create_dir_all(&preview_dir)?;
    let path = preview_dir.join(format!("{work_item_id}.html"));
    let rendered = markdown_to_html(markdown);
    std::fs::write(
        &path,
        format!(
            "<!doctype html><meta charset=\"utf-8\"><title>rq-tui preview</title>\
             <style>body{{max-width:900px;margin:3rem auto;font:16px/1.55 system-ui}}\
             pre{{padding:1rem;background:#111827;color:#e5e7eb;overflow:auto}}\
             code{{font-family:ui-monospace,monospace}}</style><body>{rendered}</body>"
        ),
    )?;
    Ok(path)
}

fn platform_preview_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

fn markdown_to_html(markdown: &str) -> String {
    let mut output = String::new();
    let mut in_code = false;
    let mut in_list = false;
    let lines = markdown.lines().collect::<Vec<_>>();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim_start().starts_with("```") {
            if in_list {
                output.push_str("</ul>");
                in_list = false;
            }
            output.push_str(if in_code {
                "</code></pre>"
            } else {
                "<pre><code>"
            });
            in_code = !in_code;
            index += 1;
            continue;
        }
        if !in_code && index + 1 < lines.len() && table_separator(lines[index + 1]).is_some() {
            if let (Some(headers), Some(alignments)) =
                (table_cells(line), table_separator(lines[index + 1]))
            {
                if headers.len() == alignments.len() {
                    if in_list {
                        output.push_str("</ul>");
                        in_list = false;
                    }
                    output.push_str("<table><thead><tr>");
                    for (header, alignment) in headers.iter().zip(&alignments) {
                        output.push_str(&format!(
                            "<th style=\"text-align:{alignment}\">{}</th>",
                            render_inline_html(header)
                        ));
                    }
                    output.push_str("</tr></thead><tbody>");
                    index += 2;
                    while index < lines.len() {
                        let Some(cells) = table_cells(lines[index]) else {
                            break;
                        };
                        if cells.len() != headers.len() {
                            break;
                        }
                        output.push_str("<tr>");
                        for (cell, alignment) in cells.iter().zip(&alignments) {
                            output.push_str(&format!(
                                "<td style=\"text-align:{alignment}\">{}</td>",
                                render_inline_html(cell)
                            ));
                        }
                        output.push_str("</tr>");
                        index += 1;
                    }
                    output.push_str("</tbody></table>");
                    continue;
                }
            }
        }
        if in_code {
            output.push_str(&escape_html(line));
            output.push('\n');
        } else if let Some(heading) = line.strip_prefix("### ") {
            output.push_str(&format!("<h3>{}</h3>", render_inline_html(heading)));
        } else if let Some(heading) = line.strip_prefix("## ") {
            output.push_str(&format!("<h2>{}</h2>", render_inline_html(heading)));
        } else if let Some(heading) = line.strip_prefix("# ") {
            output.push_str(&format!("<h1>{}</h1>", render_inline_html(heading)));
        } else if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            if !in_list {
                output.push_str("<ul>");
                in_list = true;
            }
            output.push_str(&format!("<li>{}</li>", render_inline_html(item)));
        } else {
            if in_list {
                output.push_str("</ul>");
                in_list = false;
            }
            if line.is_empty() {
                output.push_str("<br>");
            } else {
                output.push_str(&format!("<p>{}</p>", render_inline_html(line)));
            }
        }
        index += 1;
    }
    if in_code {
        output.push_str("</code></pre>");
    }
    if in_list {
        output.push_str("</ul>");
    }
    output
}

fn table_cells(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let mut trimmed = trimmed;
    if let Some(without_prefix) = trimmed.strip_prefix('|') {
        trimmed = without_prefix;
    }
    if let Some(without_suffix) = trimmed.strip_suffix('|') {
        trimmed = without_suffix;
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    for (offset, character) in trimmed.char_indices() {
        if character == '|' && !table_pipe_is_escaped(trimmed, offset) {
            cells.push(cell.trim().to_owned());
            cell.clear();
        } else {
            cell.push(character);
        }
    }
    cells.push(cell.trim().to_owned());
    (cells.len() >= 2).then_some(cells)
}

fn table_pipe_is_escaped(line: &str, offset: usize) -> bool {
    line[..offset]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count()
        % 2
        == 1
}

fn table_separator(line: &str) -> Option<Vec<&'static str>> {
    let cells = table_cells(line)?;
    cells
        .into_iter()
        .map(|cell| {
            let trimmed = cell.trim();
            let left = trimmed.starts_with(':');
            let right = trimmed.ends_with(':');
            let rule = trimmed.trim_matches(':');
            if rule.len() < 3 || !rule.bytes().all(|byte| byte == b'-') {
                return None;
            }
            Some(match (left, right) {
                (true, true) => "center",
                (false, true) => "right",
                _ => "left",
            })
        })
        .collect()
}

fn finish_recovery_item(state: &mut AppState, message_id: &str) {
    state
        .pending_asks
        .retain(|message| message.id != message_id);
    state.recovery_index = state
        .recovery_index
        .min(state.pending_asks.len().saturating_sub(1));
    finish_recovery(state);
}

fn finish_recovery(state: &mut AppState) {
    let count = state.pending_asks.len()
        + state.pending_chats.len()
        + usize::from(!state.pending_comment_ids.is_empty())
        + usize::from(state.pending_context);
    state.recovery_index = state.recovery_index.min(count.saturating_sub(1));
    if count == 0 {
        state.screen = Screen::Review;
    }
}

fn follow_chat(state: &mut AppState) {
    if !state.chat_autofollow {
        return;
    }
    state.chat_cursor = state.chat.len().saturating_sub(1);
}

fn refresh_work_item(state: &mut AppState, storage: &Storage, paths: &AppPaths) -> Result<()> {
    let remote_urls = state
        .work_item
        .repos
        .iter()
        .filter_map(|repo| repo.record.remote_pr_url.as_deref())
        .collect::<Vec<_>>();
    let refreshed = if remote_urls.is_empty() {
        resolve_local(&state.work_item.item.workspace_root, None, paths, storage)?
    } else if remote_urls.len() == state.work_item.repos.len() {
        let references = remote_urls
            .iter()
            .map(|url| PrReference::parse(url))
            .collect::<Result<Vec<_>>>()?;
        RemoteResolver::default().resolve(&references, paths, storage)?
    } else {
        let local_root = storage
            .setting(&format!("mixed_local_root:{}", state.work_item.item.id))?
            .map(std::path::PathBuf::from)
            .context("mixed Work Item has no persisted local root")?;
        let local = resolve_local(&local_root, None, paths, storage)?;
        let references = remote_urls
            .iter()
            .map(|url| PrReference::parse(url))
            .collect::<Result<Vec<_>>>()?;
        let remote = RemoteResolver::default().resolve(&references, paths, storage)?;
        combine_resolved(local, remote, paths, storage)?
    };
    state.work_item = refreshed;
    state.repo_index = 0;
    state.file_index = 0;
    state.cursor = 0;
    state.scroll = 0;
    state.annotations.clear();
    state.ask_threads.clear();
    for repo in &state.work_item.repos {
        state
            .annotations
            .extend(storage.annotations_for_version(&repo.version.id)?);
    }
    for (annotation, _) in &state.annotations {
        if annotation.kind == AnnotationKind::Ask {
            state.ask_threads.insert(
                annotation.id.clone(),
                storage.ask_messages_for_annotation(&annotation.id)?,
            );
        }
    }
    state.review_scroll = 0;
    state.sync_review_cursor_to_current_file();
    load_version_choices(state, storage)?;
    Ok(())
}

fn load_version_choices(state: &mut AppState, storage: &Storage) -> Result<()> {
    let mut choices = Vec::new();
    for repo in &state.work_item.repos {
        for version in storage.versions_for_repo(&repo.record.id)? {
            let (asks, comments) = storage.annotation_counts_for_version(&version.id)?;
            choices.push(VersionChoice {
                repo_name: repo.record.name.clone(),
                version,
                asks,
                comments,
            });
        }
    }
    choices.sort_by(|left, right| right.version.created_at.cmp(&left.version.created_at));
    state.versions = choices;
    state.version_index = state
        .version_index
        .min(state.versions.len().saturating_sub(1));
    Ok(())
}

fn prompt_unseen_remote_version(state: &mut AppState) -> bool {
    let Some((index, _)) = state.versions.iter().enumerate().find(|(_, choice)| {
        choice.version.kind == crate::domain::VersionKind::Remote
            && choice.version.last_opened_at.is_none()
    }) else {
        return false;
    };
    state.previous_screen = Screen::Review;
    state.screen = Screen::Versions;
    state.version_index = index;
    state.status =
        "New remote commits detected · Enter latest or choose an older reviewed version".into();
    true
}

pub(crate) fn handle_agent_envelope(
    state: &mut AppState,
    storage: &Storage,
    envelope: AgentEventEnvelope,
) -> Result<()> {
    let AgentEventEnvelope {
        lane,
        event,
        activity,
    } = envelope;
    match event {
        LaneEvent::SideStarted { side_id, .. } => {
            if !state.side_starting
                || !matches!(
                    &lane,
                    crate::copilot::AgentLane::Side { id } if id == &side_id
                )
            {
                return Ok(());
            }
            state.main_chat = Some(std::mem::take(&mut state.chat));
            state.chat.append(&mut state.pending_side_entries);
            state.side_starting = false;
            state.side_active = true;
            state.side_session_id = Some(side_id.clone());
            state.chat_scroll = 0;
            state.chat_autofollow = true;
            state.reset_chat_semantics();
            state.agent_progress.record(
                AgentPhase::Idle,
                format!("SIDE {} ready", short_id(&side_id)),
                "Ephemeral fork active; MAIN history is unchanged",
                None,
            );
            state.status = "SIDE conversation ready · /main returns to MAIN".into();
        }
        LaneEvent::SideExited {
            side_id,
            cleanup_warning,
            ..
        } => {
            if !state.side_active && !state.side_starting {
                if let Some(warning) = cleanup_warning {
                    state.agent_progress.record(
                        AgentPhase::Failed,
                        "SIDE closed, but cleanup needs attention",
                        warning.clone(),
                        None,
                    );
                    state.status = format!("MAIN active · SIDE cleanup warning: {warning}");
                } else if state.agent_progress.phase == AgentPhase::Stopping {
                    state.agent_progress.record(
                        AgentPhase::Idle,
                        "SIDE cleanup complete",
                        "MAIN remained available while the ephemeral session closed",
                        None,
                    );
                    state.status = "MAIN active · SIDE cleanup complete".into();
                }
                return Ok(());
            }
            if state
                .side_session_id
                .as_deref()
                .is_some_and(|active| active != side_id)
            {
                return Ok(());
            }
            restore_main_surface(state);
            let detail = cleanup_warning.clone().unwrap_or_else(|| {
                "SIDE was discarded; persistent MAIN history was not changed".into()
            });
            state.agent_progress.record(
                if cleanup_warning.is_some() {
                    AgentPhase::Failed
                } else {
                    AgentPhase::Idle
                },
                if cleanup_warning.is_some() {
                    "Back on MAIN · SIDE cleanup needs attention"
                } else {
                    "Back on MAIN"
                },
                detail,
                None,
            );
            state.status = cleanup_warning
                .map(|warning| format!("Returned to MAIN · {warning}"))
                .unwrap_or_else(|| "SIDE closed · returned to MAIN".into());
        }
        LaneEvent::SideFailed { message } => {
            if !state.side_starting {
                return Ok(());
            }
            for entry in state.pending_side_entries.drain(..) {
                if let Some(id) = entry.outbound_id {
                    state.pending_outbound_ids.remove(&id);
                    state.side_outbound_ids.remove(&id);
                }
            }
            state.side_starting = false;
            state.side_active = false;
            state.reset_chat_semantics();
            state.agent_progress.queue_depth = 0;
            state.agent_progress.record(
                AgentPhase::Failed,
                "Could not start SIDE",
                message.clone(),
                None,
            );
            state.status = message;
        }
        LaneEvent::SideCancelled => {
            for id in state.side_outbound_ids.drain() {
                state.pending_outbound_ids.remove(&id);
            }
            state.pending_side_entries.clear();
            state.side_starting = false;
            state.side_active = false;
            state.reset_chat_semantics();
            state.agent_progress.queue_depth = 0;
            state.agent_progress.record(
                AgentPhase::Idle,
                "SIDE creation cancelled",
                "Still on MAIN; no side message entered persistent history",
                None,
            );
            state.status = "SIDE creation cancelled · MAIN is unchanged".into();
        }
        LaneEvent::Agent(agent_event) => {
            if matches!(
                &agent_event,
                AgentEvent::PruneSessionsStarted { .. }
                    | AgentEvent::PruneSessionProgress { .. }
                    | AgentEvent::PruneRecoveryStarted { .. }
                    | AgentEvent::PruneSessionsComplete { .. }
                    | AgentEvent::PruneRecovery { .. }
            ) {
                if lane != crate::copilot::AgentLane::Main {
                    return Ok(());
                }
                handle_agent_event(state, storage, agent_event)?;
                return Ok(());
            }
            let fatal_side = matches!(&agent_event, AgentEvent::Error(_) | AgentEvent::Stopped)
                && (state.side_active || state.side_starting);
            if fatal_side {
                restore_main_surface(state);
                handle_agent_event(state, storage, agent_event)?;
                state.status = format!("MAIN restored · {}", state.status);
                return Ok(());
            }
            let lane_is_visible = match &lane {
                crate::copilot::AgentLane::Main => !state.side_active,
                crate::copilot::AgentLane::Side { id } => {
                    state.side_active && state.side_session_id.as_deref() == Some(id.as_str())
                }
            };
            if !lane_is_visible {
                let globally_visible =
                    matches!(&agent_event, AgentEvent::Error(_) | AgentEvent::Stopped);
                if lane == crate::copilot::AgentLane::Main && state.side_active {
                    handle_parked_main_event(state, storage, agent_event.clone())?;
                }
                if globally_visible {
                    handle_agent_event(state, storage, agent_event)?;
                }
                return Ok(());
            }
            if let Some(activity) = activity {
                let phase = match activity.kind {
                    ActivityKind::Intent | ActivityKind::Reasoning | ActivityKind::Retry => {
                        AgentPhase::Planning
                    }
                    ActivityKind::ToolStart
                    | ActivityKind::ToolProgress
                    | ActivityKind::ToolComplete => AgentPhase::Tool,
                    ActivityKind::Failure => AgentPhase::Failed,
                    ActivityKind::Other => state.agent_progress.phase,
                };
                let detail = match (&activity.tool, &activity.detail) {
                    (Some(tool), Some(detail)) => format!("{tool}: {detail}"),
                    (Some(tool), None) => tool.clone(),
                    (None, Some(detail)) => detail.clone(),
                    (None, None) => "SDK activity event".into(),
                };
                let lane_label = lane.label();
                let outbound_id = agent_event_outbound_id(&agent_event).or_else(|| {
                    state
                        .agent_progress
                        .phase
                        .is_active()
                        .then(|| state.agent_progress.active_outbound_id.clone())
                        .flatten()
                });
                state.agent_progress.record(
                    phase,
                    format!("{lane_label} · {}", activity.label),
                    detail,
                    outbound_id,
                );
                state.agent_activity = activity.label.clone();
                state.status = format!("{} · {}", lane.label(), activity.label);
                return Ok(());
            }
            handle_agent_event(state, storage, agent_event)?;
        }
    }
    Ok(())
}

/// Apply a MAIN event while SIDE owns the visible transcript. The parked MAIN
/// conversation must continue to stream and settle in the background, but its
/// cursor, scroll, and progress updates must not make the SIDE surface jump.
fn handle_parked_main_event(
    state: &mut AppState,
    storage: &Storage,
    event: AgentEvent,
) -> Result<()> {
    let Some(mut main_chat) = state.main_chat.take() else {
        return Ok(());
    };
    let side_chat = std::mem::replace(&mut state.chat, std::mem::take(&mut main_chat));
    let visible_chat_layout = state.chat_layout.clone();
    let visible_chat_navigation = state.chat_navigation.clone();
    let visible_chat_selection = state.chat_selection.clone();
    let visible_chat_display_rows = state.chat_display_rows.clone();
    let visible_chat_cursor = state.chat_cursor;
    let visible_chat_scroll = state.chat_scroll;
    let visible_chat_total_rows = state.chat_total_rows;
    let visible_chat_viewport_rows = state.chat_viewport_rows;
    let visible_chat_autofollow = state.chat_autofollow;
    let visible_status = state.status.clone();
    let visible_agent_activity = state.agent_activity.clone();
    let visible_agent_progress = state.agent_progress.clone();

    let result = handle_agent_event(state, storage, event);
    main_chat = std::mem::replace(&mut state.chat, side_chat);
    state.main_chat = Some(main_chat);
    state.chat_layout = visible_chat_layout;
    state.chat_navigation = visible_chat_navigation;
    state.chat_selection = visible_chat_selection;
    state.chat_display_rows = visible_chat_display_rows;
    state.chat_cursor = visible_chat_cursor;
    state.chat_scroll = visible_chat_scroll;
    state.chat_total_rows = visible_chat_total_rows;
    state.chat_viewport_rows = visible_chat_viewport_rows;
    state.chat_autofollow = visible_chat_autofollow;
    state.status = visible_status;
    state.agent_activity = visible_agent_activity;
    state.agent_progress = visible_agent_progress;
    result
}

fn restore_main_surface(state: &mut AppState) {
    if let Some(main_chat) = state.main_chat.take() {
        state.chat = main_chat;
    }
    state.pending_side_entries.clear();
    state.side_starting = false;
    state.side_active = false;
    state.side_session_id = None;
    for id in state.side_outbound_ids.drain() {
        state.pending_outbound_ids.remove(&id);
    }
    state.reset_chat_semantics();
}

fn fail_pending_prune(state: &mut AppState, message: String) {
    let Some(pending) = state.pending_prune.as_ref() else {
        return;
    };
    if state.ready_prune.is_none() {
        state.ready_prune = Some(ReadyPrune {
            request_id: pending.request_id.clone(),
            outcomes: pending
                .work_item_ids
                .iter()
                .map(|work_item_id| PruneSessionOutcome {
                    work_item_id: work_item_id.clone(),
                    remote_deleted: false,
                    local_deleted: false,
                    error: Some(message.clone()),
                })
                .collect(),
        });
    }
}

pub(crate) fn handle_agent_event(
    state: &mut AppState,
    storage: &Storage,
    event: AgentEvent,
) -> Result<()> {
    match event {
        AgentEvent::SessionReady {
            session_id,
            resumed,
            resume_warning,
        } => {
            storage.activate_session(&SessionRecord {
                id: session_id,
                work_item_id: state.work_item.item.id.clone(),
                parent_id: None,
                active: true,
                created_at: now(),
            })?;
            state.agent_connected = true;
            state.agent_activity.clear();
            state.agent_progress.record(
                AgentPhase::Idle,
                if resumed {
                    "Persistent Copilot session resumed"
                } else {
                    "Copilot SDK session connected"
                },
                resume_warning
                    .clone()
                    .unwrap_or_else(|| "Ready for a message".into()),
                None,
            );
            state.status = if let Some(warning) = resume_warning {
                warning
            } else if resumed {
                "Copilot session resumed with conversation history".into()
            } else {
                "Copilot session connected".into()
            };
        }
        AgentEvent::HistoryLoaded(history) => {
            if state.chat.is_empty() {
                state.chat = history
                    .into_iter()
                    .map(|entry| ChatEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        role: entry.role,
                        text: entry.text,
                        streaming: false,
                        annotation_id: None,
                        outbound_id: None,
                        error: None,
                    })
                    .collect();
                follow_chat(state);
            }
        }
        AgentEvent::Queued {
            outbound_id,
            position,
        } => {
            state.agent_progress.queue_depth = state
                .agent_progress
                .queue_depth
                .max(position.saturating_add(1));
            state.agent_progress.record_queued(
                format!("Queued request #{}", position + 1),
                format!(
                    "Waiting for the SDK to start outbound {}",
                    short_id(&outbound_id)
                ),
            );
            state.status = format!("Agent message queued at position {}", position + 1);
        }
        AgentEvent::QueueCancelled { outbound_id } => {
            storage.delete_queued_chat(&outbound_id)?;
            state.pending_outbound_ids.remove(&outbound_id);
            state.side_outbound_ids.remove(&outbound_id);
            state.agent_progress.queue_depth = state.agent_progress.queue_depth.saturating_sub(1);
            if let Some(message) = state
                .chat
                .iter_mut()
                .chain(state.pending_side_entries.iter_mut())
                .chain(state.main_chat.iter_mut().flatten())
                .find(|message| message.outbound_id.as_deref() == Some(outbound_id.as_str()))
            {
                message.error = Some("cancelled before start".into());
            }
            if state.agent_progress.active_outbound_id.is_none()
                && state.pending_outbound_ids.is_empty()
            {
                state.agent_progress.record(
                    AgentPhase::Idle,
                    "Queue is empty",
                    "The selected prompt was cancelled before Copilot started it",
                    None,
                );
            } else {
                state.agent_progress.record_queued(
                    "Queued prompt cancelled",
                    format!("Outbound {} will not run", short_id(&outbound_id)),
                );
            }
            state.scroll = state
                .scroll
                .min(state.queue_entry_ids().len().saturating_sub(1));
            state.status = format!("Cancelled queued prompt {}", short_id(&outbound_id));
        }
        AgentEvent::QueueReplaced {
            outbound_id,
            replacement_id,
            position,
        } => {
            let replacement_text = state
                .chat
                .iter()
                .chain(state.pending_side_entries.iter())
                .chain(state.main_chat.iter().flatten())
                .find(|message| message.outbound_id.as_deref() == Some(replacement_id.as_str()))
                .map(|message| message.text.clone());
            if storage
                .pending_chats(&state.work_item.item.id)?
                .iter()
                .any(|chat| chat.id == outbound_id)
            {
                storage.replace_queued_chat(
                    &outbound_id,
                    &PendingChat {
                        id: replacement_id.clone(),
                        work_item_id: state.work_item.item.id.clone(),
                        text: replacement_text.unwrap_or_default(),
                        kind: "chat".into(),
                        lane: "main".into(),
                        created_at: now(),
                    },
                )?;
            }
            if let Some(message) = state
                .chat
                .iter_mut()
                .chain(state.pending_side_entries.iter_mut())
                .chain(state.main_chat.iter_mut().flatten())
                .find(|message| message.outbound_id.as_deref() == Some(outbound_id.as_str()))
            {
                message.error = Some("replaced before start".into());
            }
            state.pending_outbound_ids.remove(&outbound_id);
            state.pending_outbound_ids.insert(replacement_id.clone());
            state.status = format!(
                "Queued prompt replaced atomically at position {}",
                position + 1
            );
            state.agent_progress.record_queued(
                "Queued prompt replaced",
                format!(
                    "{} → {} at queue position {}",
                    short_id(&outbound_id),
                    short_id(&replacement_id),
                    position + 1
                ),
            );
        }
        AgentEvent::QueueReplaceRejected {
            outbound_id,
            replacement_id,
            original_active,
            reason,
        } => {
            state.pending_outbound_ids.remove(&replacement_id);
            if original_active {
                state.pending_outbound_ids.insert(outbound_id.clone());
            } else {
                state.pending_outbound_ids.remove(&outbound_id);
            }
            if state.side_outbound_ids.remove(&replacement_id) && original_active {
                state.side_outbound_ids.insert(outbound_id.clone());
            }
            if let Some(message) = state
                .chat
                .iter_mut()
                .chain(state.pending_side_entries.iter_mut())
                .chain(state.main_chat.iter_mut().flatten())
                .find(|message| message.outbound_id.as_deref() == Some(outbound_id.as_str()))
            {
                message.error = None;
            }
            if let Some(message) = state
                .chat
                .iter_mut()
                .chain(state.pending_side_entries.iter_mut())
                .chain(state.main_chat.iter_mut().flatten())
                .find(|message| message.outbound_id.as_deref() == Some(replacement_id.as_str()))
            {
                message.error = Some(format!("queue edit rejected: {reason}"));
            }
            state.status = format!("Queue edit rejected: {reason}");
            state.agent_progress.record_queued(
                "Queued prompt was not replaced",
                if original_active {
                    format!(
                        "Original request {} is already active",
                        short_id(&outbound_id)
                    )
                } else {
                    format!(
                        "Original request {} already left the queue; no replacement was added",
                        short_id(&outbound_id)
                    )
                },
            );
        }
        AgentEvent::SteeringAccepted {
            steering_id,
            active_outbound_id,
        } => {
            storage.delete_queued_chat(&steering_id)?;
            state.pending_outbound_ids.remove(&steering_id);
            state.side_outbound_ids.remove(&steering_id);
            let detail = format!(
                "Correction {} is attached to turn {}",
                short_id(&steering_id),
                short_id(&active_outbound_id)
            );
            if state.agent_progress.active_outbound_id.as_deref()
                == Some(active_outbound_id.as_str())
            {
                state.agent_progress.record(
                    AgentPhase::Planning,
                    "Steering accepted by Copilot",
                    detail,
                    Some(active_outbound_id),
                );
                state.status =
                    "Steering accepted immediately · the active response is continuing".into();
            } else {
                state
                    .agent_progress
                    .record_observation("Late steering acknowledgement", detail);
                state.status =
                    "Steering was accepted for an earlier turn · current work is unchanged".into();
            }
        }
        AgentEvent::SteeringFailed {
            steering_id,
            active_outbound_id,
            message,
        } => {
            storage.delete_queued_chat(&steering_id)?;
            state.pending_outbound_ids.remove(&steering_id);
            state.side_outbound_ids.remove(&steering_id);
            if let Some(chat) = state
                .chat
                .iter_mut()
                .chain(state.pending_side_entries.iter_mut())
                .chain(state.main_chat.iter_mut().flatten())
                .find(|entry| entry.outbound_id.as_deref() == Some(steering_id.as_str()))
            {
                chat.error = Some(format!("steering failed: {message}"));
            }
            if state.agent_progress.active_outbound_id.as_deref()
                == Some(active_outbound_id.as_str())
            {
                state.agent_progress.record(
                    AgentPhase::Responding,
                    "Steering was not accepted",
                    format!("{message} · the original response is still active"),
                    Some(active_outbound_id),
                );
                state.status =
                    format!("Steering failed: {message} · the original response is still running");
            } else {
                state.agent_progress.record_observation(
                    "Late steering failure",
                    format!(
                        "Turn {}: {message} · current work is unchanged",
                        short_id(&active_outbound_id)
                    ),
                );
                state.status =
                    "Steering failed for an earlier turn · current work is unchanged".into();
            }
        }
        AgentEvent::ResponseStarted {
            outbound_id,
            outbound,
            first_delta,
        } => {
            let first_delta_len = first_delta.len();
            let annotation_id = match &outbound {
                OutboundKind::Ask {
                    annotation_id,
                    user_message_id,
                    assistant_message_id,
                    assistant_seq,
                } => {
                    if !state
                        .annotations
                        .iter()
                        .any(|(annotation, _)| annotation.id == *annotation_id)
                    {
                        state.pending_outbound_ids.remove(&outbound_id);
                        state.status = "Ignored a late Copilot response for a deleted Ask".into();
                        return Ok(());
                    }
                    let response = AskMessage {
                        id: assistant_message_id.clone(),
                        annotation_id: annotation_id.clone(),
                        seq: *assistant_seq,
                        role: "assistant".into(),
                        text: first_delta.clone(),
                        sent: true,
                        delivery_state: DeliveryState::Sent,
                        ts: now(),
                    };
                    storage.acknowledge_ask_with_response_start(user_message_id, &response)?;
                    if let Some(user) =
                        state.ask_threads.get_mut(annotation_id).and_then(|thread| {
                            thread
                                .iter_mut()
                                .find(|message| message.id == *user_message_id)
                        })
                    {
                        user.sent = true;
                        user.delivery_state = DeliveryState::Sent;
                    }
                    state
                        .ask_threads
                        .entry(annotation_id.clone())
                        .or_default()
                        .push(response);
                    if let Some((annotation, _)) = state
                        .annotations
                        .iter_mut()
                        .find(|(annotation, _)| annotation.id == *annotation_id)
                    {
                        annotation.delivery_state = DeliveryState::Sent;
                    }
                    Some(annotation_id.clone())
                }
                OutboundKind::CommentBatch { annotation_ids } => {
                    storage.mark_comments_delivery(annotation_ids, DeliveryState::Sent)?;
                    state
                        .pending_comment_ids
                        .retain(|id| !annotation_ids.contains(id));
                    for (annotation, _) in &mut state.annotations {
                        if annotation_ids.contains(&annotation.id) {
                            annotation.delivery_state = DeliveryState::Sent;
                            annotation.submitted = true;
                        }
                    }
                    None
                }
                OutboundKind::ContextDraft => {
                    state.context_draft = first_delta.clone();
                    state.context_streaming = true;
                    None
                }
                OutboundKind::Context { work_item_id } => {
                    storage.mark_context_sent(work_item_id)?;
                    state.pending_context = false;
                    None
                }
                OutboundKind::Chat | OutboundKind::Correction => {
                    storage.delete_queued_chat(&outbound_id)?;
                    None
                }
            };
            state.chat.push(ChatEntry {
                id: uuid::Uuid::new_v4().to_string(),
                role: "copilot".into(),
                text: first_delta,
                streaming: true,
                annotation_id,
                outbound_id: Some(outbound_id.clone()),
                error: None,
            });
            follow_chat(state);
            state.agent_activity = "Responding…".into();
            state.agent_progress.queue_depth = state.agent_progress.queue_depth.saturating_sub(1);
            state.agent_progress.record(
                AgentPhase::Responding,
                "Copilot is streaming a response",
                if first_delta_len == 0 {
                    "Turn started; waiting for the first visible text".into()
                } else {
                    format!("Received {first_delta_len} response bytes")
                },
                Some(outbound_id.clone()),
            );
            state.status = "Copilot is responding…".into();
        }
        AgentEvent::ResponseDelta { outbound_id, delta } => {
            state.agent_progress.record(
                AgentPhase::Responding,
                "Copilot is streaming a response",
                format!(
                    "Received another {} bytes for outbound {}",
                    delta.len(),
                    short_id(&outbound_id)
                ),
                Some(outbound_id.clone()),
            );
            if state.context_streaming {
                state.context_draft.push_str(&delta);
            }
            if let Some(message) = state.chat.iter_mut().find(|message| {
                message.streaming && message.outbound_id.as_deref() == Some(outbound_id.as_str())
            }) {
                message.text.push_str(&delta);
                if let Some(annotation_id) = &message.annotation_id {
                    storage.update_latest_ask_response(annotation_id, &message.text)?;
                    if let Some(response) =
                        state.ask_threads.get_mut(annotation_id).and_then(|thread| {
                            thread
                                .iter_mut()
                                .rev()
                                .find(|entry| entry.role == "assistant")
                        })
                    {
                        response.text = message.text.clone();
                    }
                }
            }
            follow_chat(state);
        }
        AgentEvent::ResponseSnapshot { outbound_id, text } => {
            state.agent_progress.record(
                AgentPhase::Responding,
                "Copilot refreshed the response snapshot",
                format!("Snapshot now contains {} bytes", text.len()),
                Some(outbound_id.clone()),
            );
            if state.context_streaming {
                state.context_draft = text.clone();
            }
            if let Some(message) = state.chat.iter_mut().find(|message| {
                message.streaming && message.outbound_id.as_deref() == Some(outbound_id.as_str())
            }) {
                message.text = text;
                if let Some(annotation_id) = &message.annotation_id {
                    storage.update_latest_ask_response(annotation_id, &message.text)?;
                    if let Some(response) =
                        state.ask_threads.get_mut(annotation_id).and_then(|thread| {
                            thread
                                .iter_mut()
                                .rev()
                                .find(|entry| entry.role == "assistant")
                        })
                    {
                        response.text = message.text.clone();
                    }
                }
            }
            follow_chat(state);
        }
        AgentEvent::ResponseComplete {
            outbound_id,
            aborted,
        } => {
            storage.delete_queued_chat(&outbound_id)?;
            state.pending_outbound_ids.remove(&outbound_id);
            state.side_outbound_ids.remove(&outbound_id);
            let completed_visible_turn = state.agent_progress.active_outbound_id.as_deref()
                == Some(outbound_id.as_str())
                || state.agent_progress.active_outbound_id.is_none();
            let context_completed = state.context_streaming;
            if context_completed {
                state.context_streaming = false;
                state.status = if aborted {
                    "Context generation stopped; the partial draft is editable".into()
                } else {
                    "Context draft ready; edit or accept it".into()
                };
            }
            let mut response_marked = false;
            if let Some(message) = state.chat.iter_mut().find(|message| {
                message.streaming && message.outbound_id.as_deref() == Some(outbound_id.as_str())
            }) {
                message.streaming = false;
                if aborted {
                    message.error = Some("stopped".into());
                }
                response_marked = true;
            }
            if aborted && !response_marked {
                if let Some(message) =
                    state.chat.iter_mut().rev().find(|message| {
                        message.outbound_id.as_deref() == Some(outbound_id.as_str())
                    })
                {
                    message.error = Some("cancelled before response start".into());
                }
            }
            if !context_completed && completed_visible_turn {
                state.status = if aborted {
                    "Copilot response stopped".into()
                } else {
                    "Copilot response complete".into()
                };
            }
            if completed_visible_turn {
                state.agent_activity.clear();
                state.agent_progress.record(
                    AgentPhase::Idle,
                    if aborted {
                        "Copilot response aborted"
                    } else {
                        "Copilot response complete"
                    },
                    format!("Outbound {} reached SDK idle", short_id(&outbound_id)),
                    None,
                );
            }
        }
        AgentEvent::StopSettledAlreadyIdle => {
            if let Some(outbound_id) = state.agent_progress.active_outbound_id.clone() {
                handle_agent_event(
                    state,
                    storage,
                    AgentEvent::ResponseComplete {
                        outbound_id,
                        aborted: false,
                    },
                )?;
            }
            state.agent_activity.clear();
            state.agent_progress.record(
                AgentPhase::Idle,
                "Copilot was already idle",
                "The stop request raced with SDK completion; no response remains to cancel",
                None,
            );
            state.status =
                "Copilot was already idle when the stop request arrived · nothing is stuck".into();
        }
        AgentEvent::TurnFailed {
            outbound_id,
            outbound,
            message,
            response_started,
        } => {
            if response_started {
                storage.delete_queued_chat(&outbound_id)?;
            }
            state.pending_outbound_ids.remove(&outbound_id);
            state.side_outbound_ids.remove(&outbound_id);
            if outbound == OutboundKind::ContextDraft {
                state.context_streaming = false;
            }
            if let Some(chat) = state
                .chat
                .iter_mut()
                .rev()
                .find(|entry| entry.outbound_id.as_deref() == Some(outbound_id.as_str()))
            {
                chat.streaming = false;
                chat.error = Some(message.clone());
            }
            match outbound {
                OutboundKind::Ask { annotation_id, .. } => {
                    if let Some((annotation, _)) = state
                        .annotations
                        .iter_mut()
                        .find(|(annotation, _)| annotation.id == annotation_id)
                    {
                        annotation.delivery_state = if response_started {
                            DeliveryState::Sent
                        } else {
                            DeliveryState::Pending
                        };
                    }
                }
                OutboundKind::CommentBatch { annotation_ids } => {
                    state.pending_comment_ids = annotation_ids;
                }
                OutboundKind::Context { .. } => state.pending_context = true,
                OutboundKind::Chat | OutboundKind::ContextDraft | OutboundKind::Correction => {}
            }
            state.agent_activity.clear();
            state.agent_progress.record(
                AgentPhase::Failed,
                "Copilot turn failed",
                message.clone(),
                Some(outbound_id.clone()),
            );
            state.status = format!("Copilot turn failed: {message}");
        }
        AgentEvent::Activity { outbound_id, label } => {
            state.agent_activity = label.clone();
            let outbound_id = outbound_id.or_else(|| {
                state
                    .agent_progress
                    .phase
                    .is_active()
                    .then(|| state.agent_progress.active_outbound_id.clone())
                    .flatten()
            });
            let phase = if outbound_id.is_some() {
                AgentPhase::Planning
            } else {
                state.agent_progress.phase
            };
            state
                .agent_progress
                .record(phase, label.clone(), "Copilot SDK activity", outbound_id);
            state.status = label;
        }
        AgentEvent::Usage {
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
        } => {
            state.last_usage = Some(format!(
                "{model} · in {} · out {} · cache {}",
                input_tokens
                    .map(|tokens| tokens.to_string())
                    .unwrap_or_else(|| "?".into()),
                output_tokens
                    .map(|tokens| tokens.to_string())
                    .unwrap_or_else(|| "?".into()),
                cache_read_tokens
                    .map(|tokens| tokens.to_string())
                    .unwrap_or_else(|| "?".into()),
            ));
        }
        AgentEvent::Forked {
            parent_id,
            session_id,
        } => {
            storage.activate_session(&SessionRecord {
                id: session_id,
                work_item_id: state.work_item.item.id.clone(),
                parent_id: Some(parent_id),
                active: true,
                created_at: now(),
            })?;
            state.agent_connected = true;
            state.agent_activity.clear();
            state.agent_progress.record(
                AgentPhase::Idle,
                "Persistent session fork activated",
                "New MAIN session is ready",
                None,
            );
            state.status = "Forked and activated a new session".into();
        }
        AgentEvent::ModelsListed(models) => {
            state.model_options = models;
            state.model_picker_index = state
                .model_options
                .iter()
                .position(|model| model.id == state.model)
                .unwrap_or(0);
            state.status = if state.model_options.is_empty() {
                "Copilot runtime returned no selectable models".into()
            } else {
                format!(
                    "Loaded {} runtime model choices · select one to continue",
                    state.model_options.len()
                )
            };
        }
        AgentEvent::ModelSelectionChanged(selection) => {
            persist_model_preferences(state, storage, &selection)?;
            state.model = selection.model_id.clone();
            state.reasoning_effort = selection.reasoning_effort.clone();
            state.context_tier = selection.context_tier.clone();
            state.status = format!(
                "Model changed to {} · reasoning {} · context {}",
                selection.model_id,
                selection
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("runtime default"),
                selection
                    .context_tier
                    .as_deref()
                    .unwrap_or("runtime default"),
            );
        }
        AgentEvent::ModelSelectionFailed { selection, message } => {
            state.agent_progress.record(
                AgentPhase::Failed,
                "Model selection failed",
                format!(
                    "{} was rejected: {message} · active model remains {}",
                    selection.model_id, state.model
                ),
                state.agent_progress.active_outbound_id.clone(),
            );
            state.status = format!(
                "Could not switch to {}: {message} · still using {} · retry with :model",
                selection.model_id, state.model
            );
        }
        AgentEvent::ModelChanged(model) => {
            state.model = model.clone();
            state.status = format!("Model changed to {model}");
        }
        AgentEvent::Compacted => state.status = "Session compacted".into(),
        AgentEvent::OrphanSideCleanup {
            session_id,
            cleanup_warning,
        } => {
            if let Some(warning) = cleanup_warning {
                state.agent_progress.record(
                    AgentPhase::Failed,
                    "SIDE cleanup needs attention",
                    warning.clone(),
                    None,
                );
                state.status = format!("MAIN ready · SIDE cleanup warning: {warning}");
            } else {
                state.agent_progress.record(
                    AgentPhase::Idle,
                    format!("Cleaned interrupted SIDE {}", short_id(&session_id)),
                    "The stale ephemeral session was deleted; MAIN was not changed",
                    None,
                );
                state.status = format!(
                    "MAIN ready · interrupted SIDE {} cleaned",
                    short_id(&session_id)
                );
            }
        }
        AgentEvent::PruneSessionsStarted {
            request_id,
            work_items,
        } => {
            if state
                .pending_prune
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id)
            {
                state.agent_progress.record(
                    AgentPhase::Tool,
                    "Deleting Copilot sessions",
                    format!(
                        "Cleanup started for {work_items} Work Item(s); local history is still intact"
                    ),
                    None,
                );
                state.status =
                    format!("Copilot session cleanup started for {work_items} Work Item(s)…");
            }
        }
        AgentEvent::PruneRecoveryStarted {
            operation_id,
            work_item_id,
        } => {
            state.prune_recoveries.insert(operation_id);
            state.agent_progress.record(
                AgentPhase::Tool,
                "Recovering interrupted prune",
                format!("{work_item_id} · loading the durable cleanup journal"),
                None,
            );
            state.status = format!("Recovering interrupted prune for {work_item_id}…");
        }
        AgentEvent::PruneSessionProgress {
            request_id,
            work_item_id,
            label,
            completed,
            total,
        } => {
            let interactive = state
                .pending_prune
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id);
            let recovering = state.prune_recoveries.contains(&request_id);
            if interactive || recovering {
                let detail = if total == 0 {
                    format!("{work_item_id} · {label}")
                } else {
                    format!("{work_item_id} · {label} · {completed}/{total}")
                };
                state.agent_progress.record(
                    AgentPhase::Tool,
                    if interactive {
                        "Deleting Copilot sessions"
                    } else {
                        "Recovering interrupted prune"
                    },
                    detail.clone(),
                    None,
                );
                state.status = detail;
            }
        }
        AgentEvent::PruneSessionsComplete {
            request_id,
            outcomes,
        } => {
            if state
                .pending_prune
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id)
            {
                state.ready_prune = Some(ReadyPrune {
                    request_id,
                    outcomes,
                });
                state.agent_progress.record(
                    AgentPhase::Tool,
                    "Copilot session cleanup finished",
                    "Finalizing local Work Item history cleanup",
                    None,
                );
                state.status =
                    "Copilot session cleanup finished · finalizing local history…".into();
            }
        }
        AgentEvent::PruneRecovery {
            operation_id,
            outcome,
        } => {
            if !state.prune_recoveries.remove(&operation_id) {
                return Ok(());
            }
            let work_item_id = outcome.work_item_id;
            if let Some(error) = outcome.error {
                if outcome.local_deleted {
                    state.prune_items.retain(|item| item.id != work_item_id);
                }
                state.agent_progress.record(
                    AgentPhase::Failed,
                    "Interrupted prune still needs attention",
                    if outcome.local_deleted {
                        format!(
                            "{work_item_id}: local history is already deleted; remote/journal cleanup remains unresolved: {error}"
                        )
                    } else {
                        format!("{work_item_id}: local history was retained: {error}")
                    },
                    None,
                );
                state.status = if outcome.local_deleted {
                    format!(
                        "Could not finish prune for {work_item_id} · local history deleted · cleanup still unresolved"
                    )
                } else {
                    format!("Could not resume prune for {work_item_id} · local history retained")
                };
            } else {
                state.prune_items.retain(|item| item.id != work_item_id);
                state.agent_progress.record(
                    AgentPhase::Idle,
                    "Interrupted prune recovered",
                    format!("{work_item_id}: remote and local cleanup completed"),
                    None,
                );
                state.status = format!("Recovered interrupted prune for {work_item_id}");
            }
        }
        AgentEvent::Error(error) => {
            fail_pending_prune(
                state,
                format!("Copilot worker failed during prune: {error}"),
            );
            state.prune_recoveries.clear();
            state.agent_connected = false;
            state.agent_activity = "Disconnected".into();
            state.agent_progress.record(
                AgentPhase::Disconnected,
                "Copilot SDK disconnected",
                error.clone(),
                None,
            );
            for message in state
                .chat
                .iter_mut()
                .chain(state.main_chat.iter_mut().flatten())
                .filter(|message| message.streaming)
            {
                message.streaming = false;
                message.error = Some("connection lost".into());
            }
            state.status = format!("Copilot unavailable: {error}");
        }
        AgentEvent::Stopped => {
            fail_pending_prune(
                state,
                "Copilot worker stopped before prune completion; local history was retained".into(),
            );
            state.prune_recoveries.clear();
            let cleanup_warning = (state.agent_progress.summary == "SIDE cleanup needs attention")
                .then(|| state.agent_progress.detail.clone());
            state.agent_connected = false;
            state.agent_activity = "Disconnected".into();
            state.agent_progress.record(
                AgentPhase::Disconnected,
                "Copilot worker stopped",
                cleanup_warning
                    .as_ref()
                    .map(|warning| {
                        format!("No SDK event stream is active · SIDE cleanup warning: {warning}")
                    })
                    .unwrap_or_else(|| "No SDK event stream is active".into()),
                None,
            );
            for message in state
                .chat
                .iter_mut()
                .chain(state.main_chat.iter_mut().flatten())
                .filter(|message| message.streaming)
            {
                message.streaming = false;
                message.error = Some("worker stopped".into());
            }
            state.status = cleanup_warning
                .map(|warning| {
                    format!(
                        "Copilot worker stopped · SIDE cleanup warning: {warning} · restart to resume"
                    )
                })
                .unwrap_or_else(|| {
                    "Copilot worker stopped · restart to resume the session".into()
                });
        }
    }
    Ok(())
}

fn parse_review_context(work_item_id: &str, draft: &str) -> ReviewContext {
    let mut context = ReviewContext {
        work_item_id: work_item_id.to_owned(),
        source: "generated".into(),
        ..ReviewContext::default()
    };
    for line in draft.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_owned();
        match label.trim().to_ascii_lowercase().as_str() {
            "title" => context.title = value,
            "what" => context.what = value,
            "why" => context.why = value,
            "how" => context.how = value,
            "considerations" => context.considerations = value,
            "other approaches" | "alternatives" => context.alternatives = value,
            _ => {}
        }
    }
    context
}

fn render_review_context(context: &ReviewContext) -> String {
    format!(
        "Title: {}\nWhat: {}\nWhy: {}\nHow: {}\nConsiderations: {}\nOther approaches: {}",
        context.title,
        context.what,
        context.why,
        context.how,
        context.considerations,
        context.alternatives,
    )
}

pub(crate) fn render(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    highlighter: &mut dyn Highlighter,
) {
    if frame.area().width < 40 || frame.area().height < 9 {
        let area = frame.area();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(format!(
                "needs at least 40×9\ncurrent: {}×{}\nresize to continue\n:q still exits safely",
                area.width, area.height
            ))
            .block(
                Block::default()
                    .title(" Terminal too small ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    state.viewport_height = frame.area().height.saturating_sub(6) as usize;
    match state.screen {
        Screen::Review => render_review(frame, state, highlighter),
        Screen::Chat => render_chat(frame, state, highlighter),
        Screen::Settings => render_settings(frame, state),
        Screen::Versions => render_versions(frame, state),
        Screen::ContextEditor => render_context_editor(frame, state),
        Screen::Prune => render_prune(frame, state),
        Screen::Recovery => render_recovery(frame, state),
        Screen::AgentStatus => render_agent_status(frame, state),
        Screen::Queue => render_queue(frame, state),
        Screen::ModelPicker => render_model_picker(frame, state),
        Screen::Preview => render_markdown_preview(frame, state, highlighter),
    }
    if state.input_mode == InputMode::Command && state.screen != Screen::Chat {
        render_command_palette(frame, state);
    }
}

fn render_markdown_preview(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    highlighter: &mut dyn Highlighter,
) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).max(1) as usize;
    let rendered = state
        .preview_markdown
        .as_deref()
        .map(|markdown| render_markdown_mapped(markdown, width, highlighter))
        .unwrap_or_else(|| MappedMarkdown {
            rows: Vec::new(),
            elided: Vec::new(),
        });
    let lines = rendered
        .rows
        .iter()
        .map(|row| row.line.clone())
        .collect::<Vec<_>>();

    let preview_block = Block::default()
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan))
        .borders(Borders::ALL);
    let inner = preview_block.inner(area);
    let footer_height = usize::from(inner.height > 1);
    let content_height = inner.height.saturating_sub(footer_height as u16);
    state.preview_total_rows = lines.len();
    state.preview_scroll = state
        .preview_scroll
        .min(lines.len().saturating_sub(content_height.max(1) as usize));
    let first_row = if lines.is_empty() {
        0
    } else {
        state.preview_scroll + 1
    };
    let last_row = (state.preview_scroll + content_height.max(1) as usize).min(lines.len());
    let block = preview_block.title(format!(
        " Markdown preview · rows {first_row}-{last_row}/{} ",
        lines.len(),
    ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let content = Rect::new(inner.x, inner.y, inner.width, content_height);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((state.preview_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false }),
        content,
    );
    if footer_height > 0 {
        let footer = if inner.width < 60 {
            " j/k · C-u/d · C-f/b · gg/G · o browser · q/Esc "
        } else {
            " j/k · C-u/d half · C-f/b page · gg/G · o browser · q/Esc "
        };
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
}

fn render_recovery(frame: &mut ratatui::Frame, state: &AppState) {
    let mut items = state
        .pending_asks
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let marker = if index == state.recovery_index {
                "▶"
            } else {
                " "
            };
            ListItem::new(format!(
                "{marker} {} — {}",
                message.ts,
                message.text.replace('\n', " ")
            ))
        })
        .collect::<Vec<_>>();
    let mut next_index = state.pending_asks.len();
    for chat in &state.pending_chats {
        let marker = if next_index == state.recovery_index {
            "▶"
        } else {
            " "
        };
        let label = if chat.kind == "correction" {
            format!("{} correction", chat.lane.to_uppercase())
        } else {
            "queued Chat".into()
        };
        items.push(ListItem::new(format!(
            "{marker} {label} — {}",
            chat.text.replace('\n', " ")
        )));
        next_index += 1;
    }
    if !state.pending_comment_ids.is_empty() {
        let marker = if next_index == state.recovery_index {
            "▶"
        } else {
            " "
        };
        items.push(ListItem::new(format!(
            "{marker} comment batch — {} comments",
            state.pending_comment_ids.len()
        )));
        next_index += 1;
    }
    if state.pending_context {
        let marker = if next_index == state.recovery_index {
            "▶"
        } else {
            " "
        };
        items.push(ListItem::new(format!(
            "{marker} accepted Work Item context"
        )));
    }
    let area = frame.area();
    let total = items.len();
    let viewport = usize::from(area.height.saturating_sub(3)).max(1);
    let start = state
        .recovery_index
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(total.saturating_sub(viewport));
    let end = (start + viewport).min(total);
    let items = items
        .into_iter()
        .skip(start)
        .take(viewport)
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(
                    " Delivery recovery · may be undelivered · {}-{}/{} ",
                    total.min(start + 1),
                    end,
                    total
                ))
                .borders(Borders::ALL),
        ),
        area,
    );
    let footer = Rect::new(
        area.x.saturating_add(1),
        area.bottom().saturating_sub(2),
        area.width.saturating_sub(2),
        1,
    );
    frame.render_widget(
        Paragraph::new("j/k select · r resend intentionally · d discard · q defer"),
        footer,
    );
}

fn render_context_editor(frame: &mut ratatui::Frame, state: &AppState) {
    let title = if state.context_streaming {
        "Generate Context — drafting…"
    } else {
        "Generate Context"
    };
    let body = if state.context_draft.is_empty() {
        "Waiting for the read-only agent…".to_owned()
    } else {
        state.context_draft.clone()
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        frame.area(),
    );
    let area = frame.area();
    frame.render_widget(
        Paragraph::new("e edit · a accept & attach · r regenerate · q discard"),
        Rect::new(
            area.x.saturating_add(1),
            area.bottom().saturating_sub(2),
            area.width.saturating_sub(2),
            1,
        ),
    );
}

fn render_settings(frame: &mut ratatui::Frame, state: &AppState) {
    let area = frame.area();
    let base = state
        .work_item
        .repos
        .get(state.repo_index)
        .and_then(|repo| repo.record.base_branch.as_deref())
        .unwrap_or("auto-detect");
    let rows = [
        format!(
            "Model             {} · reasoning {} · context {}",
            state.model,
            state
                .reasoning_effort
                .as_deref()
                .unwrap_or("runtime default"),
            state.context_tier.as_deref().unwrap_or("runtime default"),
        ),
        "Ask tool scope    read/search only (fixed)".into(),
        format!(
            "Diff layout       {} (launch default)",
            state.default_layout.label()
        ),
        format!("Base branch       {base} (per repository)"),
        format!("Cache dir         {}", state.cache_directory),
        "gh auth           managed by gh CLI for remote reviews".into(),
        "Keybindings       vim (built in)".into(),
        format!(
            "Diff context      6 lines · expand step {}",
            state.expand_step
        ),
        format!(
            "File tree default {}",
            if state.file_tree_default_open {
                "open"
            } else {
                "closed"
            }
        ),
        format!(
            "Markdown preview  {} (o always opens browser)",
            state.markdown_preview.label()
        ),
        format!("Skills dir        {}", state.skill_directories),
        format!("Storage           SQLite (WAL) · {}", state.storage_path),
    ];
    let row_count = rows.len();
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    let first_row = state
        .settings_index
        .saturating_add(1)
        .saturating_sub(visible_rows);
    let last_row = (first_row + visible_rows).min(row_count);
    let items = rows
        .into_iter()
        .enumerate()
        .skip(first_row)
        .take(visible_rows)
        .map(|(index, row)| {
            let line = format!(
                "{} {row}",
                if index == state.settings_index {
                    "❯"
                } else {
                    " "
                }
            );
            ListItem::new(truncate_terminal_line(
                &line,
                usize::from(area.width.saturating_sub(2)),
            ))
        })
        .collect::<Vec<_>>();
    let title = if area.width < 80 {
        format!(
            "Settings {}-{}/{} · Enter · j/k · q",
            first_row + 1,
            last_row,
            row_count
        )
    } else {
        format!(
            "Settings · rows {}-{}/{} · Enter edit/toggle · j/k · q",
            first_row + 1,
            last_row,
            row_count
        )
    };
    frame.render_widget(
        List::new(items).block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn truncate_terminal_line(text: &str, max_cells: usize) -> String {
    if cell_width(text) <= max_cells {
        return text.to_owned();
    }
    if max_cells == 0 {
        return String::new();
    }
    let mut rendered = String::new();
    let mut used: usize = 0;
    for (_, grapheme) in grapheme_indices(text) {
        let width = cell_width(grapheme);
        if used.saturating_add(width).saturating_add(1) > max_cells {
            break;
        }
        rendered.push_str(grapheme);
        used += width;
    }
    rendered.push('…');
    rendered
}

fn render_model_picker(frame: &mut ratatui::Frame, state: &AppState) {
    let compact = frame.area().width < 60;
    let (step, title, subtitle, rows) = match state.model_picker_stage {
        ModelPickerStage::Model => (
            1,
            "Choose a model",
            "Choices reported by Copilot",
            state
                .model_options
                .iter()
                .map(|model| {
                    let context = model
                        .max_context_tokens
                        .map(format_token_count)
                        .unwrap_or_else(|| "runtime default".into());
                    let efforts = if model.supported_reasoning_efforts.is_empty() {
                        "fixed reasoning".into()
                    } else {
                        model.supported_reasoning_efforts.join("/")
                    };
                    if compact {
                        format!("{} [{}] · {context} · {efforts}", model.name, model.id)
                    } else {
                        format!(
                            "{:<24} {:<24} · context {context} · {efforts}",
                            model.name, model.id
                        )
                    }
                })
                .collect::<Vec<_>>(),
        ),
        ModelPickerStage::Reasoning => {
            let model = state
                .pending_model_selection
                .as_ref()
                .and_then(|selection| {
                    state
                        .model_options
                        .iter()
                        .find(|model| model.id == selection.model_id)
                });
            (
                2,
                "Choose reasoning effort",
                "Levels supported by this model",
                model
                    .map(|model| {
                        model
                            .supported_reasoning_efforts
                            .iter()
                            .map(|effort| {
                                if model.default_reasoning_effort.as_deref()
                                    == Some(effort.as_str())
                                {
                                    format!("{effort} · runtime default")
                                } else {
                                    effort.clone()
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        }
        ModelPickerStage::Context => {
            let model = state
                .pending_model_selection
                .as_ref()
                .and_then(|selection| {
                    state
                        .model_options
                        .iter()
                        .find(|model| model.id == selection.model_id)
                });
            (
                3,
                "Choose context tier",
                "Runtime-advertised capacity",
                model
                    .map(|model| {
                        model
                            .context_tiers
                            .iter()
                            .map(|tier| {
                                format!(
                                    "{} · {}",
                                    tier.id,
                                    tier.max_context_tokens
                                        .map(format_token_count)
                                        .unwrap_or_else(|| "runtime default".into())
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        }
    };
    let viewport = frame.area().height.saturating_sub(6).max(1) as usize;
    let start = state
        .model_picker_index
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(rows.len().saturating_sub(viewport));
    let items = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, row)| {
            let selected = index == state.model_picker_index;
            ListItem::new(format!("{} {row}", if selected { "▶" } else { " " })).style(
                if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )
        })
        .collect::<Vec<_>>();
    let items = if items.is_empty() {
        vec![ListItem::new("Loading model capabilities from Copilot…")]
    } else {
        items
    };
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(" Model picker · step {step}/3 · {title} "))
                .borders(Borders::ALL),
        ),
        frame.area(),
    );
    let area = frame.area();
    let footer = if area.width < 60 {
        "↑/↓ select · Enter next · Esc cancel".to_owned()
    } else {
        format!("{subtitle} · ↑/↓ select · Enter next · Esc cancel")
    };
    frame.render_widget(
        Paragraph::new(fit_terminal_text(
            &footer,
            area.width.saturating_sub(2) as usize,
        ))
        .style(Style::default().fg(Color::DarkGray)),
        Rect::new(
            area.x.saturating_add(1),
            area.bottom().saturating_sub(2),
            area.width.saturating_sub(2),
            1,
        ),
    );
}

fn format_token_count(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tokens", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k tokens", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens} tokens")
    }
}

fn render_versions(frame: &mut ratatui::Frame, state: &AppState) {
    let area = frame.area();
    let row_width = usize::from(area.width.saturating_sub(2));
    let rows = state
        .versions
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let marker = if index == state.version_index {
                "▶"
            } else {
                " "
            };
            let prefix = match choice.version.kind {
                crate::domain::VersionKind::WorkingTree => "v0".to_owned(),
                crate::domain::VersionKind::Remote => {
                    format!("v{}", choice.version.version_num)
                }
                crate::domain::VersionKind::Snapshot => {
                    format!("s{}", choice.version.version_num)
                }
            };
            let reviewed = choice
                .version
                .last_opened_at
                .as_deref()
                .map(|opened| format!("reviewed {opened}"))
                .unwrap_or_else(|| "NEW · not reviewed".into());
            let text = format!(
                "{marker} {} · {prefix} · {reviewed} · {} asks · {} comments",
                choice.repo_name, choice.asks, choice.comments
            );
            ListItem::new(truncate_terminal_line(&text, row_width)).style(
                if index == state.version_index {
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
    let total = rows.len();
    let viewport = usize::from(area.height.saturating_sub(3)).max(1);
    let start = state
        .version_index
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(total.saturating_sub(viewport));
    let end = (start + viewport).min(total);
    let items = rows
        .into_iter()
        .skip(start)
        .take(viewport)
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(
                    " Version History · {}-{}/{} ",
                    total.min(start + 1),
                    end,
                    total
                ))
                .borders(Borders::ALL),
        ),
        area,
    );
    frame.render_widget(
        Paragraph::new("j/k select · Enter open · q/Esc back")
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(
            area.x.saturating_add(1),
            area.bottom().saturating_sub(2),
            area.width.saturating_sub(2),
            1,
        ),
    );
}

fn render_prune(frame: &mut ratatui::Frame, state: &AppState) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(frame.area());
    let row_width = areas[0].width.saturating_sub(2) as usize;
    let items = state
        .prune_items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let cursor = if index == state.prune_index {
                "▶"
            } else {
                " "
            };
            let is_open = item.id == state.work_item.item.id;
            let selected = if item.selected { "x" } else { " " };
            let availability = if is_open {
                " OPEN(disabled)"
            } else {
                ""
            };
            let text = format!(
                "{cursor} [{selected}]{availability} {} · reviewed {} · {} versions · {} annotations",
                item.name, item.last_opened_at, item.versions, item.annotations
            );
            let row = ListItem::new(truncate_terminal_line(&text, row_width));
            if is_open {
                row.style(Style::default().fg(Color::DarkGray))
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    let title = if areas[0].width < 72 {
        "Prune · Space select · d delete · x export"
    } else {
        "Prune · Space select · d delete · x export+delete · open item disabled"
    };
    let mut list_state = ListState::default()
        .with_selected((!state.prune_items.is_empty()).then_some(state.prune_index));
    frame.render_stateful_widget(
        List::new(items).block(Block::default().title(title).borders(Borders::ALL)),
        areas[0],
        &mut list_state,
    );
    render_agent_progress(frame, state, areas[1]);
}

fn render_review(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    highlighter: &mut dyn Highlighter,
) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);
    render_header(frame, state, vertical[0]);

    if state.picker_open && vertical[1].width < 72 {
        render_picker(frame, state, vertical[1]);
        render_status(frame, state, vertical[2]);
        return;
    }
    let body = if state.picker_open {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(28), Constraint::Min(1)])
            .split(vertical[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(0), Constraint::Min(1)])
            .split(vertical[1])
    };
    if state.picker_open {
        render_picker(frame, state, body[0]);
    }
    match state.layout {
        DiffLayout::Unified => render_unified(frame, state, body[1], highlighter),
        DiffLayout::Split => render_split(frame, state, body[1], highlighter),
    }
    render_status(frame, state, vertical[2]);
}

fn render_header(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let repo = state
        .work_item
        .repos
        .get(state.repo_index)
        .map(|repo| repo.record.name.as_str())
        .unwrap_or("-");
    let file = state
        .current_file()
        .map(|file| file.path().display().to_string())
        .unwrap_or_else(|| "no changed files".into());
    let context_title = state
        .context_draft
        .lines()
        .find_map(|line| line.strip_prefix("Title:").map(str::trim))
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            state
                .work_item
                .repos
                .get(state.repo_index)
                .and_then(|repo| repo.record.pr_meta_json.as_deref())
                .and_then(|metadata| serde_json::from_str::<serde_json::Value>(metadata).ok())
                .and_then(|metadata| {
                    metadata
                        .get("title")
                        .and_then(|title| title.as_str())
                        .map(str::to_owned)
                })
        })
        .map(|title| format!("  │  {title}"))
        .unwrap_or_default();
    let focus = match state.focus {
        crate::app::Focus::FilePicker => "files",
        crate::app::Focus::Diff => "diff",
        crate::app::Focus::Chat => "chat",
        crate::app::Focus::InlineAsk => "inline ask",
    };
    let title = format!(
        " {} — Review  │  Focus: {}  │  {} > {}  │  {}/{} files{} ",
        state.work_item.item.name,
        focus,
        repo,
        file,
        state.file_index.saturating_add(1),
        state
            .current_diff()
            .map(|diff| diff.files.len())
            .unwrap_or(0),
        context_title,
    );
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn render_picker(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let filtering = state.input_mode == InputMode::Search
        && state.focus == crate::app::Focus::FilePicker
        && !state.search.is_empty();
    let mut rows: Vec<(ListItem<'static>, bool)> = Vec::new();
    for (repo_index, repo) in state.work_item.repos.iter().enumerate() {
        let collapsed = state.collapsed_repos.contains(&repo.record.id) && !filtering;
        let selected = collapsed && repo_index == state.repo_index;
        let activity = compact_repo_activity(repo.record.last_activity_at.as_deref());
        let summary = format!(
            "{} {} ({} files){}",
            if collapsed { "▸" } else { "▾" },
            repo.record.name,
            repo.diff.files.len(),
            activity
                .as_deref()
                .map(|activity| format!(" · {activity}"))
                .unwrap_or_default()
        );
        rows.push((
            ListItem::new(Line::styled(
                format!("{} {summary}", if selected { "▶" } else { " " }),
                if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                },
            )),
            selected,
        ));
        if collapsed {
            continue;
        }
        for (file_index, file) in repo.diff.files.iter().enumerate() {
            if filtering
                && !fuzzy_match(
                    &file.display_path.to_string_lossy().to_lowercase(),
                    &state.search.to_lowercase(),
                )
            {
                continue;
            }
            let selected = repo_index == state.repo_index && file_index == state.file_index;
            rows.push((
                ListItem::new(file_picker_line(
                    file,
                    selected,
                    usize::from(area.width.saturating_sub(2)),
                ))
                .style(if selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                }),
                selected,
            ));
        }
    }
    let focused = state.focus == crate::app::Focus::FilePicker;
    let selected_row = rows.iter().position(|(_, selected)| *selected);
    let viewport = usize::from(area.height.saturating_sub(1)).max(1);
    let start = selected_row
        .map(|selected| {
            selected
                .saturating_add(1)
                .saturating_sub(viewport)
                .min(rows.len().saturating_sub(viewport))
        })
        .unwrap_or(0);
    let end = (start + viewport).min(rows.len());
    let total = rows.len();
    let items = rows
        .into_iter()
        .skip(start)
        .take(viewport)
        .map(|(item, _)| item)
        .collect::<Vec<_>>();
    let range = if total > viewport {
        format!(" · {}-{}/{}", start + 1, end, total)
    } else {
        String::new()
    };
    let filter = if filtering {
        format!(" · /{}", state.search)
    } else {
        String::new()
    };
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(
                    "{} files{}{}",
                    if focused { "▶" } else { "" },
                    range,
                    filter,
                ))
                .border_style(Style::default().fg(if focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
                .borders(Borders::RIGHT),
        ),
        area,
    );
}

fn file_picker_line(file: &DiffFile, selected: bool, width: usize) -> Line<'static> {
    let (badge, color) = match file.status {
        FileStatus::Added => ("A", Color::Green),
        FileStatus::Deleted => ("D", Color::Red),
        FileStatus::Renamed => ("R", Color::Yellow),
        FileStatus::Modified => ("M", Color::Blue),
    };
    let additions = file
        .visible_lines()
        .filter(|line| line.kind == LineKind::Addition)
        .count();
    let deletions = file
        .visible_lines()
        .filter(|line| line.kind == LineKind::Deletion)
        .count();
    let marker = if selected { "▶ " } else { "  " };
    let badge = format!("{badge} ");
    let counts = format!(" +{additions} -{deletions}");
    let fixed_width = cell_width(marker) + cell_width(&badge) + cell_width(&counts);
    let path = fit_terminal_text(
        &file.display_path.to_string_lossy(),
        width.saturating_sub(fixed_width),
    );
    let gap = " ".repeat(
        width
            .saturating_sub(fixed_width)
            .saturating_sub(cell_width(&path)),
    );
    Line::from(vec![
        Span::styled(
            marker.to_owned(),
            if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
        Span::styled(badge, Style::default().fg(color)),
        Span::styled(
            path,
            if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
        Span::raw(gap),
        Span::styled(counts, Style::default().fg(Color::DarkGray)),
    ])
}

fn compact_repo_activity(value: Option<&str>) -> Option<String> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value?).ok()?;
    let age = chrono::Utc::now()
        .signed_duration_since(timestamp.with_timezone(&chrono::Utc))
        .max(chrono::Duration::zero());
    Some(if age.num_seconds() < 60 {
        "now".into()
    } else if age.num_minutes() < 60 {
        format!("{}m", age.num_minutes())
    } else if age.num_hours() < 24 {
        format!("{}h", age.num_hours())
    } else {
        format!("{}d", age.num_days())
    })
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut wanted = needle.chars();
    let mut next = wanted.next();
    for character in haystack.chars() {
        if next == Some(character) {
            next = wanted.next();
            if next.is_none() {
                return true;
            }
        }
    }
    next.is_none()
}

fn render_unified(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    area: Rect,
    highlighter: &mut dyn Highlighter,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    if area.height <= 5 && render_compact_inline_composer(frame, state, area) {
        return;
    }
    let focused = state.focus == crate::app::Focus::Diff;
    frame.render_widget(
        Paragraph::new(if focused { "▶ unified" } else { "unified" }).style(
            Style::default()
                .fg(if focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height == 1 {
        return;
    }
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    state.viewport_height = body.height.max(1) as usize;
    state.compose_wrap_width = body.width.saturating_sub(4).max(1) as usize;
    state.set_review_content_width(body.width.saturating_sub(9) as usize);
    let stream = state.review_stream();
    let (layout, semantic_rows, composer_cursor) =
        review_display_layout(stream.rows(), body.width.max(1) as usize);
    clamp_review_display_scroll(
        state,
        &semantic_rows,
        body.height.max(1) as usize,
        composer_cursor,
    );
    let viewport_end = state.review_scroll.saturating_add(body.height as usize);
    let mut lines = Vec::with_capacity(body.height as usize);
    for display in layout.iter().filter(|display| {
        display.start < viewport_end
            && display.start.saturating_add(display.height) > state.review_scroll
    }) {
        let row = &stream.rows()[display.semantic_row];
        let skip = state.review_scroll.saturating_sub(display.start);
        let take = viewport_end
            .saturating_sub(display.start.max(state.review_scroll))
            .min(display.height.saturating_sub(skip));
        lines.extend(
            review_row_lines(
                row,
                review_row_selection(state, row, display.semantic_row),
                body.width.max(1) as usize,
                state.review_horizontal_scroll,
                highlighter,
            )
            .into_iter()
            .skip(skip)
            .take(take),
        );
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), body);
}

fn render_split(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    area: Rect,
    highlighter: &mut dyn Highlighter,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    if area.height <= 5 && render_compact_inline_composer(frame, state, area) {
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let focused = state.focus == crate::app::Focus::Diff;
    let diff_border = Style::default().fg(if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    });
    frame.render_widget(
        Paragraph::new(if focused { "▶ - old" } else { "- old" })
            .style(diff_border.add_modifier(Modifier::BOLD)),
        Rect::new(columns[0].x, columns[0].y, columns[0].width, 1),
    );
    frame.render_widget(
        Paragraph::new(if focused { "▶ + new" } else { "+ new" })
            .style(diff_border.add_modifier(Modifier::BOLD)),
        Rect::new(columns[1].x, columns[1].y, columns[1].width, 1),
    );
    if area.height == 1 {
        return;
    }

    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    let body_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body);
    state.viewport_height = body.height.max(1) as usize;
    state.compose_wrap_width = body.width.saturating_sub(4).max(1) as usize;
    state.set_review_content_width(
        body_columns[0]
            .width
            .saturating_sub(7)
            .min(body_columns[1].width.saturating_sub(6)) as usize,
    );
    let stream = state.review_stream();
    let selection_side = state.review_selection_side();
    let (layout, semantic_rows, composer_cursor) =
        review_display_layout(stream.rows(), body.width.max(1) as usize);
    clamp_review_display_scroll(
        state,
        &semantic_rows,
        body.height.max(1) as usize,
        composer_cursor,
    );
    let viewport_end = state.review_scroll.saturating_add(body.height as usize);
    let mut rendered_rows = Vec::new();
    for display in layout.iter().filter(|display| {
        display.start < viewport_end
            && display.start.saturating_add(display.height) > state.review_scroll
    }) {
        let index = display.semantic_row;
        let row = &stream.rows()[index];
        let selection = review_row_selection(state, row, index);
        let skip = state.review_scroll.saturating_sub(display.start);
        let take = viewport_end
            .saturating_sub(display.start.max(state.review_scroll))
            .min(display.height.saturating_sub(skip));
        match row {
            ReviewRow::Source {
                file,
                line,
                kind,
                old_line,
                new_line,
                content,
                ..
            } => {
                let source = DiffLine {
                    kind: *kind,
                    old_line: *old_line,
                    new_line: *new_line,
                    content: content.clone(),
                };
                let (old, new) = match kind {
                    LineKind::Deletion => (Some(&source), None),
                    LineKind::Addition => (None, Some(&source)),
                    LineKind::Context => (Some(&source), Some(&source)),
                    LineKind::Meta => (None, None),
                };
                let old = clip_styled_line_content(
                    split_line(
                        old,
                        Some(std::path::Path::new(file)),
                        old.and(selection.filter(|_| selection_side == Some(AnchorSide::Old))),
                        highlighter,
                    ),
                    1,
                    state.review_horizontal_scroll,
                    body_columns[0].width.saturating_sub(1) as usize,
                );
                let new = clip_styled_line_content(
                    split_line(
                        new,
                        Some(std::path::Path::new(file)),
                        new.and(selection.filter(|_| selection_side == Some(AnchorSide::New))),
                        highlighter,
                    ),
                    1,
                    state.review_horizontal_scroll,
                    body_columns[1].width as usize,
                );
                if skip == 0 && take > 0 {
                    rendered_rows.push((index, old, new, None));
                }
                let _ = line;
            }
            _ => {
                for line in review_row_lines(
                    row,
                    selection,
                    body.width as usize,
                    state.review_horizontal_scroll,
                    highlighter,
                )
                .into_iter()
                .skip(skip)
                .take(take)
                {
                    rendered_rows.push((index, Line::from(""), Line::from(""), Some(line)));
                }
            }
        }
    }
    let old_lines = rendered_rows
        .iter()
        .map(|(_, old, _, _)| old.clone())
        .collect::<Vec<_>>();
    let new_lines = rendered_rows
        .iter()
        .map(|(_, _, new, _)| new.clone())
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(old_lines).block(
            Block::default()
                .border_style(diff_border)
                .borders(Borders::RIGHT),
        ),
        body_columns[0],
    );
    frame.render_widget(Paragraph::new(new_lines), body_columns[1]);
    for (offset, (_, _, _, line)) in rendered_rows.into_iter().enumerate() {
        let Some(line) = line else {
            continue;
        };
        let line_area = Rect::new(body.x, body.y + offset as u16, body.width, 1);
        frame.render_widget(Clear, line_area);
        frame.render_widget(Paragraph::new(line), line_area);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReviewDisplayRow {
    semantic_row: usize,
    start: usize,
    height: usize,
}

fn review_display_layout(
    rows: &[ReviewRow],
    width: usize,
) -> (Vec<ReviewDisplayRow>, Vec<usize>, Option<usize>) {
    let mut layout = Vec::with_capacity(rows.len());
    let mut semantic_rows = Vec::with_capacity(rows.len());
    let mut composer_cursor = None;
    let mut start = 0usize;
    for (semantic_row, row) in rows.iter().enumerate() {
        let (height, cursor_line) = review_row_geometry(row, width);
        let height = height.max(1);
        layout.push(ReviewDisplayRow {
            semantic_row,
            start,
            height,
        });
        semantic_rows.extend(std::iter::repeat_n(semantic_row, height));
        if let Some(cursor_line) = cursor_line {
            composer_cursor = Some(start.saturating_add(cursor_line));
        }
        start = start.saturating_add(height);
    }
    (layout, semantic_rows, composer_cursor)
}

fn review_row_geometry(row: &ReviewRow, width: usize) -> (usize, Option<usize>) {
    let ReviewRow::Annotation { block, .. } = row else {
        return (1, None);
    };
    let AnnotationRowPart::Body { .. } = block.part else {
        return (1, None);
    };
    let available = width.saturating_sub(4).max(1);
    let lines = wrapped_editor_lines(&block.text, block.text.len(), available).0;
    let cursor = (block.annotation_id == INLINE_COMPOSER_ID)
        .then(|| lines.iter().position(|line| line.contains('▏')))
        .flatten();
    (lines.len().max(1), cursor)
}

fn clamp_review_display_scroll(
    state: &mut AppState,
    semantic_rows: &[usize],
    viewport: usize,
    composer_cursor: Option<usize>,
) {
    if let Some(cursor) = composer_cursor {
        if cursor < state.review_scroll {
            state.review_scroll = cursor;
        } else if cursor >= state.review_scroll.saturating_add(viewport) {
            state.review_scroll = cursor.saturating_add(1).saturating_sub(viewport);
        }
    }
    if composer_cursor.is_none() {
        let selected_start = semantic_rows
            .iter()
            .position(|row| *row == state.review_cursor);
        let selected_end = semantic_rows
            .iter()
            .rposition(|row| *row == state.review_cursor);
        if let (Some(start), Some(end)) = (selected_start, selected_end) {
            if start < state.review_scroll {
                state.review_scroll = start;
            } else if end >= state.review_scroll.saturating_add(viewport) {
                state.review_scroll = end.saturating_add(1).saturating_sub(viewport);
            }
        }
    }
    state.review_scroll = state
        .review_scroll
        .min(semantic_rows.len().saturating_sub(viewport));
}

fn render_compact_inline_composer(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    area: Rect,
) -> bool {
    let label = match state.compose_target.as_ref() {
        Some(ComposeTarget::Annotation(AnnotationKind::Ask)) => "Ask",
        Some(ComposeTarget::Annotation(AnnotationKind::Comment)) => "Comment",
        Some(ComposeTarget::FollowUp(_)) => "Ask follow-up",
        Some(ComposeTarget::EditAnnotation(_) | ComposeTarget::EditAskMessage { .. }) => {
            "Edit annotation"
        }
        _ => return false,
    };
    if state.input_mode != InputMode::Compose {
        return false;
    }

    let mut marked = state.compose.clone();
    marked.insert(floor_grapheme_boundary(&marked, state.compose_cursor), '▏');
    // `bordered_body` reserves one inner column for padding in addition to
    // the two border cells, so wrap to the exact selectable content width.
    let inner_width = area.width.saturating_sub(3).max(1) as usize;
    let display = format!("❯ {marked}");
    let (rows, _, _) = wrapped_editor_lines(&display, display.len(), inner_width);
    let cursor_row = rows
        .iter()
        .position(|row| row.contains('▏'))
        .unwrap_or(rows.len().saturating_sub(1));
    let editor_height = area.height.saturating_sub(2).max(1) as usize;
    let scroll = cursor_row
        .saturating_add(1)
        .saturating_sub(editor_height)
        .min(rows.len().saturating_sub(editor_height));
    state.compose_scroll = scroll;
    state.compose_wrap_width = inner_width;

    let title = format!(
        "▶ {label} · INSERT · {}-{}/{}",
        scroll + 1,
        (scroll + editor_height).min(rows.len()),
        rows.len()
    );
    if area.height == 1 {
        frame.render_widget(
            Paragraph::new(bordered_top(&title, area.width as usize)).style(
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
        );
        return true;
    }
    frame.render_widget(
        Paragraph::new(bordered_top(&title, area.width as usize)).style(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 2 {
        let visible = rows
            .into_iter()
            .skip(scroll)
            .take(editor_height)
            .map(|row| Line::raw(bordered_body(&row, area.width as usize)))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(visible),
            Rect::new(area.x, area.y + 1, area.width, area.height - 2),
        );
    }
    frame.render_widget(
        Paragraph::new(bordered_bottom_label(
            "↑↓ · Enter · Esc keep · ^C discard",
            area.width as usize,
        ))
        .style(Style::default().fg(Color::Cyan)),
        Rect::new(
            area.x,
            area.y + area.height.saturating_sub(1),
            area.width,
            1,
        ),
    );
    true
}

fn review_row_selection(
    state: &AppState,
    row: &ReviewRow,
    index: usize,
) -> Option<ReviewRowSelection> {
    if state.input_mode != InputMode::Visual {
        return (index == state.review_cursor).then_some(ReviewRowSelection::Whole);
    }
    let ReviewRow::Source {
        repo_id,
        file,
        old_anchor,
        new_anchor,
        ..
    } = row
    else {
        return None;
    };
    let current_repo = state.work_item.repos.get(state.repo_index)?;
    let current_file = state.current_file()?;
    if repo_id != &current_repo.record.id
        || file.as_str() != current_file.path().to_string_lossy().as_ref()
    {
        return None;
    }
    let visible_line = new_anchor
        .as_ref()
        .or(old_anchor.as_ref())
        .map(|anchor| anchor.visible_line);
    visible_line.and_then(|line| state.review_row_selection(line))
}

fn review_row_lines(
    row: &ReviewRow,
    selection: Option<ReviewRowSelection>,
    width: usize,
    horizontal_scroll: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<Line<'static>> {
    let selected = selection.is_some();
    let selected_style = matches!(selection, Some(ReviewRowSelection::Whole))
        .then_some(Style::default().bg(Color::Rgb(40, 50, 65)));
    match row {
        ReviewRow::FileHeader {
            repo_name,
            path,
            status,
            ..
        } => {
            let badge = match status {
                crate::diff::FileStatus::Added => "A",
                crate::diff::FileStatus::Modified => "M",
                crate::diff::FileStatus::Deleted => "D",
                crate::diff::FileStatus::Renamed => "R",
            };
            vec![styled_full_row(
                format!(
                    "{} {badge} {repo_name} > {path}",
                    if selected { "❯" } else { " " }
                ),
                width,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                selected_style,
            )]
        }
        ReviewRow::HunkHeader { text, .. } => vec![styled_full_row(
            format!("{} {text}", if selected { "❯" } else { " " }),
            width,
            Style::default().fg(Color::Blue),
            selected_style,
        )],
        ReviewRow::Fold {
            hidden_lines, text, ..
        } => vec![styled_full_row(
            format!(
                "{} ··· {} unchanged lines ···  {}",
                if selected { "❯" } else { " " },
                hidden_lines,
                text
            ),
            width,
            Style::default().fg(Color::Blue),
            selected_style,
        )],
        ReviewRow::Source {
            file,
            line,
            kind,
            old_line,
            new_line,
            content,
            ..
        } => {
            let source = DiffLine {
                kind: *kind,
                old_line: *old_line,
                new_line: *new_line,
                content: content.clone(),
            };
            let mut rendered = unified_line(
                std::path::Path::new(file),
                &source,
                *line,
                selected,
                highlighter,
            );
            rendered = rendered.style(match (selection, kind) {
                (Some(ReviewRowSelection::Whole), _) => Style::default().bg(Color::Rgb(40, 50, 65)),
                (_, LineKind::Addition) => Style::default().bg(Color::Rgb(18, 48, 31)),
                (_, LineKind::Deletion) => Style::default().bg(Color::Rgb(56, 25, 29)),
                _ => Style::default(),
            });
            if let Some(ReviewRowSelection::Columns { start, end }) = selection {
                paint_content_columns(&mut rendered, 1, start, end);
            }
            rendered = clip_styled_line_content(rendered, 1, horizontal_scroll, width);
            let padding = width.saturating_sub(rendered.width());
            if padding > 0 {
                rendered.spans.push(Span::raw(" ".repeat(padding)));
            }
            vec![rendered]
        }
        ReviewRow::Annotation { block, .. } => {
            let accent = Style::default().fg(Color::Cyan);
            let available = width.saturating_sub(4).max(1);
            let strings = match block.part {
                AnnotationRowPart::Header { collapsed } => {
                    let marker = if selected { "❯ " } else { "" };
                    let indicator = if collapsed { "▸ " } else { "" };
                    vec![bordered_top(
                        &format!("{marker}{indicator}{}", block.text),
                        width,
                    )]
                }
                AnnotationRowPart::Body { .. } => {
                    let wrapped = wrapped_editor_lines(&block.text, block.text.len(), available).0;
                    wrapped
                        .into_iter()
                        .map(|line| bordered_body(&line, width))
                        .collect()
                }
                AnnotationRowPart::Footer => vec![bordered_bottom(width)],
            };
            strings
                .into_iter()
                .map(|text| {
                    let mut line = Line::styled(text, accent);
                    if let Some(style) = selected_style {
                        line = line.style(style);
                    }
                    line
                })
                .collect()
        }
    }
}

fn styled_full_row(
    text: String,
    width: usize,
    style: Style,
    selected: Option<Style>,
) -> Line<'static> {
    let mut line = Line::styled(fit_terminal_text(&text, width), style);
    if let Some(selected) = selected {
        line = line.style(selected);
    }
    line
}

fn bordered_top(title: &str, width: usize) -> String {
    if width < 2 {
        return fit_terminal_text(title, width);
    }
    let inner = width.saturating_sub(2);
    let label = fit_terminal_text(&format!("─ {title} "), inner);
    format!("╭{label}╮")
}

fn bordered_body(text: &str, width: usize) -> String {
    if width < 2 {
        return fit_terminal_text(text, width);
    }
    format!("│{}│", fit_terminal_text(&format!(" {text}"), width - 2))
}

fn bordered_bottom(width: usize) -> String {
    match width {
        0 => String::new(),
        1 => "╰".into(),
        _ => format!("╰{}╯", "─".repeat(width - 2)),
    }
}

fn bordered_bottom_label(label: &str, width: usize) -> String {
    if width < 2 {
        return fit_terminal_text(label, width);
    }
    let inner = width.saturating_sub(2);
    let label = fit_terminal_text(&format!("─ {label} "), inner);
    format!("╰{label}╯")
}

fn fit_terminal_text(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0usize;
    for (_, grapheme) in grapheme_indices(text) {
        let grapheme_width = cell_width(grapheme);
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        result.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    result
}

fn truncate_styled_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    if width == 0 {
        line.spans.clear();
        return line;
    }

    let available = width.saturating_sub(1);
    let mut used = 0usize;
    let mut spans = Vec::new();
    'outer: for span in line.spans {
        let mut text = String::new();
        for (_, grapheme) in grapheme_indices(span.content.as_ref()) {
            let grapheme_width = cell_width(grapheme);
            if used.saturating_add(grapheme_width) > available {
                if !text.is_empty() {
                    spans.push(Span::styled(text, span.style));
                }
                break 'outer;
            }
            text.push_str(grapheme);
            used = used.saturating_add(grapheme_width);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    spans.push(Span::styled(
        "…",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    line.spans = spans;
    line
}

fn clip_styled_line_content(
    mut line: Line<'static>,
    prefix_spans: usize,
    horizontal_scroll: usize,
    width: usize,
) -> Line<'static> {
    if width == 0 {
        line.spans.clear();
        return line;
    }
    let prefix_len = prefix_spans.min(line.spans.len());
    let mut output = line.spans.drain(..prefix_len).collect::<Vec<_>>();
    let prefix_width = output.iter().map(|span| span.width()).sum::<usize>();
    let available = width.saturating_sub(prefix_width);
    if available == 0 {
        line.spans = output;
        return truncate_styled_line(line, width);
    }
    let content_width = line
        .spans
        .iter()
        .map(|span| cell_width(span.content.as_ref()))
        .sum::<usize>();
    let show_left = horizontal_scroll > 0;
    let left_width = usize::from(show_left);
    let show_right =
        content_width > horizontal_scroll.saturating_add(available.saturating_sub(left_width));
    let content_capacity = available
        .saturating_sub(left_width)
        .saturating_sub(usize::from(show_right));
    let marker_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    if show_left {
        output.push(Span::styled("‹", marker_style));
    }
    let mut source_column = 0usize;
    let mut used = 0usize;
    'spans: for span in line.spans {
        for (_, grapheme) in grapheme_indices(span.content.as_ref()) {
            let grapheme_width = cell_width(grapheme).max(1);
            let grapheme_end = source_column.saturating_add(grapheme_width);
            if grapheme_end <= horizontal_scroll {
                source_column = grapheme_end;
                continue;
            }
            if used.saturating_add(grapheme_width) > content_capacity {
                break 'spans;
            }
            output.push(Span::styled(grapheme.to_owned(), span.style));
            used = used.saturating_add(grapheme_width);
            source_column = grapheme_end;
        }
    }
    if show_right {
        output.push(Span::styled("…", marker_style));
    }
    line.spans = output;
    line
}

fn paint_content_columns(
    line: &mut Line<'static>,
    content_span_start: usize,
    start: usize,
    end: usize,
) {
    let mut column = 0usize;
    let prefix_len = content_span_start.min(line.spans.len());
    let mut painted = line.spans.drain(..prefix_len).collect::<Vec<_>>();
    let selection = Style::default().bg(Color::Rgb(40, 50, 65));
    for span in line.spans.drain(..) {
        for (_, grapheme) in grapheme_indices(span.content.as_ref()) {
            let width = cell_width(grapheme).max(1);
            let grapheme_end = column.saturating_add(width).saturating_sub(1);
            let style = if column <= end && grapheme_end >= start {
                span.style.patch(selection)
            } else {
                span.style
            };
            painted.push(Span::styled(grapheme.to_owned(), style));
            column = column.saturating_add(width);
        }
    }
    line.spans = painted;
}

fn render_status(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let mode = match state.input_mode {
        InputMode::Normal => "NORMAL",
        InputMode::Visual => "VISUAL",
        InputMode::Command => "COMMAND",
        InputMode::Search => "SEARCH",
        InputMode::Compose => "INSERT",
    };
    let content = if let Some(warning) = state.quit_guard.as_ref() {
        warning.clone()
    } else if !state.pending_prefix.is_empty() {
        state.status.clone()
    } else {
        match state.input_mode {
        InputMode::Command => "COMMAND  type to filter · Enter run · Esc cancel".into(),
        InputMode::Search => format!("/{:<width$}", state.search, width = area.width as usize),
        InputMode::Compose
            if matches!(
                state.compose_target,
                Some(
                    ComposeTarget::Annotation(_)
                        | ComposeTarget::FollowUp(_)
                        | ComposeTarget::EditAnnotation(_)
                        | ComposeTarget::EditAskMessage { .. }
                )
            ) =>
        {
            "INSERT  Enter/Ctrl-S submit · Shift-Enter newline · Esc keep · Ctrl-C discard".into()
        }
        InputMode::Compose => format!(
            "{mode}  {}  (Enter/Ctrl-S submit · Shift-Enter newline · Esc cancel)",
            state.compose.replace('\n', " ↵ ")
        ),
        InputMode::Visual => {
            let (start, end) = state.selection();
            let visual = state
                .review_selection_mode()
                .map(|mode| mode.label())
                .unwrap_or("SELECT");
            if area.width < 60 {
                format!(
                    "VISUAL {visual} · {}-{} · y copy · Esc clear",
                    start + 1,
                    end + 1,
                )
            } else {
                let side = state
                    .review_selection_side()
                    .map(|side| side.as_str())
                    .unwrap_or("none");
                let viewport = if state.review_horizontal_scroll > 0 {
                    format!(
                        " · view cols {}-{}",
                        state.review_horizontal_scroll + 1,
                        state.review_horizontal_scroll + state.review_content_width,
                    )
                } else {
                    String::new()
                };
                format!(
                    "VISUAL {visual} · rows {}-{} · cols {}-{} · side {side}{} · a ask · c comment · y yank · Esc clear",
                    start + 1,
                    end + 1,
                    state.review_visual_anchor_column.min(state.review_visual_column) + 1,
                    state.review_visual_anchor_column.max(state.review_visual_column) + 1,
                    viewport,
                )
            }
        }
        _ if state.status.is_empty() => {
            format!(
                "{mode}  j/k move · h/l file · t files · v select · a ask · c comment · Tab chat · : command"
            )
        }
        _ => format!(
            "{mode} · {} · j/k move · h/l file · t files · v select · a ask · c comment · Tab chat · : command",
            state.status
        ),
        }
    };
    let progress = &state.agent_progress;
    let lane = visible_lane(state);
    let liveness = format!(
        "COPILOT {lane} {} {} · last SDK event {} ago · {} · :agent-status",
        progress.phase.label(),
        format_duration(progress.elapsed()),
        format_duration(progress.last_event_age()),
        progress.summary,
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(content),
            Line::styled(
                liveness,
                Style::default().fg(if progress.phase.is_active() {
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

fn render_command_palette(frame: &mut ratatui::Frame, state: &mut AppState) {
    let area = frame.area();
    let compact = area.width < 60 || area.height < 16;
    let width = if compact {
        area.width
    } else {
        area.width.saturating_sub(4).min(96)
    };
    let height = if compact {
        area.height
    } else {
        12.min(area.height.saturating_sub(2)).max(4)
    };
    let palette = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + if compact { 0 } else { 1 },
        width,
        height,
    );
    render_command_palette_area(frame, state, palette, true);
}

fn render_command_palette_area(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    palette: Rect,
    show_input: bool,
) {
    let matches = command_matches(&state.command);
    let chrome_rows = if show_input { 4 } else { 3 };
    let visible_rows = palette.height.saturating_sub(chrome_rows) as usize;
    state.command_viewport_rows = visible_rows.max(1);
    if state.command_index < state.command_scroll {
        state.command_scroll = state.command_index;
    } else if state.command_index >= state.command_scroll + state.command_viewport_rows {
        state.command_scroll = state
            .command_index
            .saturating_add(1)
            .saturating_sub(state.command_viewport_rows);
    }
    let start = state
        .command_scroll
        .min(matches.len().saturating_sub(visible_rows));
    let suggestions = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, (command, description))| {
            let selected = index == state.command_index;
            Line::from(vec![
                Span::styled(
                    if selected { " ▶ " } else { "   " },
                    Style::default().fg(if selected {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    format!(":{command:<28}"),
                    Style::default()
                        .fg(if selected { Color::Cyan } else { Color::White })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(*description, Style::default().fg(Color::DarkGray)),
            ])
            .style(if selected {
                Style::default().bg(Color::Rgb(30, 43, 57))
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    if show_input {
        lines.push(Line::styled(
            format!("  :{}█", state.command),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if suggestions.is_empty() {
        lines.push(Line::styled(
            "   No matching commands",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        lines.extend(suggestions);
    }
    let selected_number = if matches.is_empty() {
        0
    } else {
        state.command_index.min(matches.len() - 1) + 1
    };
    let compact = palette.width < 60;
    let help = if compact {
        format!(
            " ↑/↓ Pg · Home/End · Enter · Esc  {selected_number}/{}",
            matches.len()
        )
    } else {
        format!(
            " ↑/↓ wrap · PgUp/PgDn · Home/End · Tab complete · Enter run · Esc cancel  {selected_number}/{}",
            matches.len()
        )
    };
    lines.push(Line::styled(help, Style::default().fg(Color::DarkGray)));
    frame.render_widget(Clear, palette);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(if show_input {
                    " COMMAND MODE · Command palette "
                } else {
                    " COMMAND COMPLETIONS "
                })
                .title_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .border_style(Style::default().fg(Color::Cyan))
                .borders(Borders::ALL),
        ),
        palette,
    );
}

fn render_chat(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    highlighter: &mut dyn Highlighter,
) {
    let width = frame.area().width.saturating_sub(4).max(1) as usize;
    let composer_height = if state.input_mode == InputMode::Command {
        3
    } else {
        chat_composer_height(state, width, frame.area().height)
    };
    let progress_height = if state.input_mode == InputMode::Command && frame.area().height < 14 {
        1
    } else if state.input_mode == InputMode::Command || frame.area().height < 12 {
        3
    } else if frame.area().width < 60 {
        5
    } else if frame.area().height >= 14 {
        4
    } else {
        3
    };
    let minimum_chat = if state.input_mode == InputMode::Command && frame.area().height < 14 {
        1
    } else {
        3
    };
    let command_height = if state.input_mode == InputMode::Command {
        let desired = (command_matches(&state.command).len().min(5) as u16)
            .saturating_add(3)
            .clamp(4, 8);
        desired.min(
            frame
                .area()
                .height
                .saturating_sub(progress_height)
                .saturating_sub(composer_height)
                .saturating_sub(minimum_chat),
        )
    } else {
        0
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(minimum_chat),
            Constraint::Length(progress_height),
            Constraint::Length(command_height),
            Constraint::Length(composer_height),
        ])
        .split(frame.area());

    let mut lines = chat_lines(state, width, highlighter);
    if lines.is_empty() {
        lines.push(Line::styled(
            "No session messages yet. The composer is always available below.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let viewport_rows = vertical[0].height.saturating_sub(2) as usize;
    state.chat_total_rows = lines.len();
    state.chat_viewport_rows = viewport_rows;
    let max_scroll = lines.len().saturating_sub(viewport_rows);
    if state.chat_autofollow {
        state.chat_scroll = max_scroll;
    } else {
        state.chat_scroll = state.chat_scroll.min(max_scroll);
    }
    if state.input_mode == InputMode::Visual {
        if let Some(location) = state
            .chat_layout
            .as_ref()
            .zip(state.chat_navigation.as_ref())
            .and_then(|(layout, cursor)| layout.locate(&cursor.point))
        {
            let row = state
                .chat_display_rows
                .get(location.row)
                .copied()
                .unwrap_or(location.row);
            if row < state.chat_scroll {
                state.chat_scroll = row;
            } else if row >= state.chat_scroll + viewport_rows {
                state.chat_scroll = row + 1 - viewport_rows;
            }
        }
    }
    let lane = visible_lane(state);
    let row_range = format!(
        "rows {}-{}/{}",
        state
            .chat_scroll
            .saturating_add(1)
            .min(state.chat_total_rows.max(1)),
        (state.chat_scroll + viewport_rows).min(state.chat_total_rows),
        state.chat_total_rows,
    );
    let chat_title = if frame.area().width < 60 {
        format!(" Chat · {lane} · {row_range} ")
    } else {
        format!(
            " {} — Chat · {lane} · {row_range} ",
            state.work_item.item.name
        )
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().title(chat_title).borders(Borders::ALL))
            .scroll((state.chat_scroll.min(u16::MAX as usize) as u16, 0)),
        vertical[0],
    );
    if progress_height > 0 {
        render_agent_progress(frame, state, vertical[1]);
    }
    if command_height > 0 {
        render_command_palette_area(frame, state, vertical[2], false);
    }
    if composer_height > 0 {
        render_chat_composer(frame, state, vertical[3]);
    }
}

fn chat_lines(
    state: &mut AppState,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(2).max(1);
    let mapped = state
        .chat
        .iter()
        .map(|message| render_markdown_mapped(&message.text, body_width, highlighter))
        .collect::<Vec<_>>();
    if let Some(layout) = build_chat_layout(&state.chat, &mapped, body_width) {
        state.set_chat_layout(layout, chat_display_rows(&state.chat, &mapped));
    }
    let mut lines = Vec::new();
    let mut layout_row = 0usize;
    for (index, (message, mapped_message)) in state.chat.iter().zip(mapped.iter()).enumerate() {
        let cursor_message = index == state.chat_cursor;
        let stopped = message
            .error
            .as_deref()
            .is_some_and(|error| error == "stopped" || error.starts_with("cancelled"));
        let marker = if message.streaming {
            " ◐ streaming"
        } else if stopped {
            " ■ stopped"
        } else if message.error.is_some() {
            " ⚠ failed"
        } else if message.role != "copilot"
            && message.outbound_id.is_some()
            && state.agent_progress.active_outbound_id.is_some()
            && message.outbound_id.as_deref() == state.agent_progress.active_outbound_id.as_deref()
            && state.agent_progress.phase.is_active()
        {
            " · active"
        } else if message.role != "copilot"
            && message
                .outbound_id
                .as_ref()
                .is_some_and(|id| state.pending_outbound_ids.contains(id))
        {
            " · queued"
        } else {
            ""
        };
        let lane = if state.side_active { "SIDE" } else { "MAIN" };
        lines.push(
            Line::styled(
                format!(
                    "{}{} · {lane}{marker}",
                    if cursor_message { "▶ " } else { "  " },
                    message.role
                ),
                Style::default()
                    .fg(if message.role == "copilot" {
                        Color::Cyan
                    } else {
                        Color::Green
                    })
                    .add_modifier(Modifier::BOLD),
            )
            .style(if cursor_message {
                Style::default().bg(Color::Rgb(40, 50, 65))
            } else {
                Style::default()
            }),
        );
        for row in &mapped_message.rows {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(project_mapped_row(row, layout_row, state));
            lines.push(Line::from(spans));
            layout_row = layout_row.saturating_add(1);
        }
        if let Some(error) = &message.error {
            lines.push(Line::styled(
                if stopped {
                    "  response cancelled".to_owned()
                } else {
                    format!("  error: {error}")
                },
                Style::default().fg(if stopped { Color::Yellow } else { Color::Red }),
            ));
        }
        if index + 1 < state.chat.len() {
            lines.push(Line::from(""));
        }
    }
    lines
}

fn chat_display_rows(entries: &[ChatEntry], mapped: &[MappedMarkdown]) -> Vec<usize> {
    let mut display_row = 0usize;
    let mut result = Vec::new();
    for (index, (entry, rendered)) in entries.iter().zip(mapped).enumerate() {
        display_row = display_row.saturating_add(1); // message header
        result.extend((0..rendered.rows.len()).map(|row| display_row.saturating_add(row)));
        display_row = display_row.saturating_add(rendered.rows.len());
        if entry.error.is_some() {
            display_row = display_row.saturating_add(1);
        }
        if index + 1 < entries.len() {
            display_row = display_row.saturating_add(1); // message spacer
        }
    }
    result
}

fn build_chat_layout(
    entries: &[ChatEntry],
    mapped: &[MappedMarkdown],
    width: usize,
) -> Option<ChatLayout> {
    let messages = entries
        .iter()
        .map(|entry| ChatMessage {
            id: entry.id.clone().into(),
            speaker: Some(entry.role.clone()),
            text: entry.text.clone(),
            blocks: vec![ChatBlock {
                id: BlockId(0),
                source: SelectionSourceRange::new(0, entry.text.len()),
            }],
        })
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for (entry, rendered) in entries.iter().zip(mapped) {
        for (index, row) in rendered.rows.iter().enumerate() {
            let cells = row
                .cells
                .iter()
                .map(|cell| {
                    let width = cell.columns.end.saturating_sub(cell.columns.start).max(1);
                    match &cell.source {
                        CellSource::Text(source) => ChatCell::source(
                            SelectionSourceRange::new(source.start, source.end),
                            width,
                        ),
                        CellSource::Decoration(_) | CellSource::Synthetic => {
                            ChatCell::display_only(width)
                        }
                    }
                })
                .collect::<Vec<_>>();
            let break_after = mapped_row_break(&entry.text, &rendered.rows, index);
            rows.push(ChatRow::new(
                entry.id.clone(),
                BlockId(0),
                cells,
                break_after,
            ));
        }
    }
    ChatLayout::new(width, messages, rows).ok()
}

fn mapped_row_break(text: &str, rows: &[MappedRow], index: usize) -> RowBreak {
    let Some(current) = rows.get(index) else {
        return RowBreak::End;
    };
    let Some(next) = rows.get(index.saturating_add(1)) else {
        return RowBreak::End;
    };
    let current_end = current.cells.iter().filter_map(mapped_cell_end).max();
    let next_start = next.cells.iter().filter_map(mapped_cell_start).min();
    if let (Some(end), Some(start)) = (current_end, next_start) {
        if end <= start && text.get(end..start).is_some_and(|gap| gap.contains('\n')) {
            return RowBreak::Hard;
        }
    }
    RowBreak::Soft
}

fn mapped_cell_start(cell: &crate::chat_render::MappedCell) -> Option<usize> {
    match &cell.source {
        CellSource::Text(source) | CellSource::Decoration(source) => Some(source.start),
        CellSource::Synthetic => None,
    }
}

fn mapped_cell_end(cell: &crate::chat_render::MappedCell) -> Option<usize> {
    match &cell.source {
        CellSource::Text(source) | CellSource::Decoration(source) => Some(source.end),
        CellSource::Synthetic => None,
    }
}

fn project_mapped_row(row: &MappedRow, layout_row: usize, state: &AppState) -> Vec<Span<'static>> {
    let selected_cells = row
        .cells
        .iter()
        .enumerate()
        .map(|(cell_index, _)| {
            state
                .chat_selection
                .as_ref()
                .zip(state.chat_layout.as_ref())
                .is_some_and(|(selection, layout)| {
                    selection.contains_cell(layout, layout_row, cell_index)
                })
        })
        .collect::<Vec<_>>();
    let mut column = 0usize;
    let mut first_candidate = 0usize;
    let mut output = Vec::new();
    for span in &row.line.spans {
        for character in span.content.chars() {
            let text = character.to_string();
            let width = Span::raw(text.clone()).width().max(1);
            while row
                .cells
                .get(first_candidate)
                .is_some_and(|cell| cell.columns.end <= column)
            {
                first_candidate = first_candidate.saturating_add(1);
            }
            let selected = row
                .cells
                .iter()
                .zip(&selected_cells)
                .skip(first_candidate)
                .take_while(|(cell, _)| cell.columns.start < column.saturating_add(width))
                .any(|(cell, selected)| *selected && cell.columns.end > column);
            let style = if selected {
                span.style.bg(Color::Rgb(40, 50, 65))
            } else {
                span.style
            };
            output.push(Span::styled(text, style));
            column = column.saturating_add(width);
        }
    }
    output
}

fn chat_composer_height(state: &AppState, width: usize, terminal_height: u16) -> u16 {
    let inner_width = width.saturating_sub(2).max(1);
    let line_count = wrapped_editor_lines(&state.compose, state.compose_cursor, inner_width)
        .0
        .len()
        .max(1);
    let desired = line_count.saturating_add(2) as u16;
    let cap = (terminal_height / 3).clamp(3, 12);
    desired.clamp(3, cap)
}

fn render_chat_composer(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    state.compose_wrap_width = inner_width;
    let (lines, cursor_row, cursor_col) =
        wrapped_editor_lines(&state.compose, state.compose_cursor, inner_width);
    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    if cursor_row < state.compose_scroll {
        state.compose_scroll = cursor_row;
    } else if cursor_row >= state.compose_scroll + visible_rows {
        state.compose_scroll = cursor_row + 1 - visible_rows;
    }
    state.compose_scroll = state
        .compose_scroll
        .min(lines.len().saturating_sub(visible_rows));

    let queued_edit_id = match state.compose_target.as_ref() {
        Some(ComposeTarget::EditQueued(outbound_id)) => Some(short_id(outbound_id)),
        _ => None,
    };
    let (title, border_color) = match state.input_mode {
        InputMode::Compose if queued_edit_id.is_some() => {
            let id = queued_edit_id.unwrap_or_default();
            let range = format!(
                "{}-{}/{}",
                state.compose_scroll + 1,
                (state.compose_scroll + visible_rows).min(lines.len()),
                lines.len()
            );
            (
                if area.width < 60 {
                    format!(" EDIT QUEUED {id} · {range} · Enter replace ")
                } else {
                    format!(
                        " EDIT QUEUED {id} · rows {range} · ↑/↓ scroll · Enter replaces · Esc keeps "
                    )
                },
                Color::Yellow,
            )
        }
        InputMode::Compose if state.status.starts_with("Pasted ") => {
            let range = format!(
                "{}-{}/{}",
                state.compose_scroll + 1,
                (state.compose_scroll + visible_rows).min(lines.len()),
                lines.len()
            );
            (
                if area.width < 60 {
                    format!(" PASTE {range} · ↑/↓ · Enter · Esc ")
                } else {
                    format!(
                        " {} · rows {range} · ↑/↓ scroll · Enter send · Esc keep ",
                        state.status
                    )
                },
                Color::Green,
            )
        }
        InputMode::Compose if lines.len() > visible_rows => {
            let range = format!(
                "{}-{}/{}",
                state.compose_scroll + 1,
                (state.compose_scroll + visible_rows).min(lines.len()),
                lines.len()
            );
            (
                if area.width < 60 {
                    format!(" INSERT {range} · Enter send · Esc keep ")
                } else {
                    format!(" INSERT · rows {range} · ↑/↓ scroll · Enter send · Esc keep ")
                },
                Color::Green,
            )
        }
        InputMode::Compose => (
            if area.width < 60 {
                " INSERT · Enter send · Esc keep ".to_owned()
            } else {
                " INSERT · Enter send · Shift-Enter newline · Esc keep draft · Ctrl-C stop/discard "
                    .to_owned()
            },
            Color::Green,
        ),
        InputMode::Command => (
            if area.width < 60 {
                if state.compose.is_empty() {
                    " COMMAND MODE ACTIVE · Esc cancel ".to_owned()
                } else {
                    format!(
                        " COMMAND MODE ACTIVE · draft {}B held ",
                        state.compose.len()
                    )
                }
            } else if state.compose.is_empty() {
                " COMMAND MODE ACTIVE ↑ USE PALETTE · Esc cancel ".to_owned()
            } else {
                format!(
                    " COMMAND MODE ACTIVE ↑ USE PALETTE · draft {} bytes held · Esc restores ",
                    state.compose.len()
                )
            },
            Color::Cyan,
        ),
        InputMode::Search => (
            " SEARCH MODE · Enter next · Esc cancel ".to_owned(),
            Color::Yellow,
        ),
        InputMode::Visual => {
            let kind = match state.chat_selection_mode() {
                Some(crate::chat_selection::ChatSelectionMode::Character) => "CHAR",
                Some(crate::chat_selection::ChatSelectionMode::Line) => "LINE",
                Some(crate::chat_selection::ChatSelectionMode::Block) => "BLOCK",
                None => "SELECT",
            };
            (
                format!(" VISUAL {kind} · h/l/j/k move · y copy · Esc normal "),
                Color::Magenta,
            )
        }
        InputMode::Normal => {
            let title = if let Some(warning) = state.quit_guard.as_ref() {
                format!(" {warning} ")
            } else if state.status.is_empty() {
                " NORMAL · i edit · j/k scroll · G latest · Tab review · Ctrl-C stop ".to_owned()
            } else if area.width < 60 {
                format!(" NORMAL · {} ", state.status)
            } else {
                format!(" NORMAL · {} · i edit · : commands ", state.status)
            };
            (title, Color::DarkGray)
        }
    };
    let display = if state.input_mode == InputMode::Command {
        let command = terminal_text_tail(&state.command, inner_width.saturating_sub(2));
        vec![Line::styled(
            format!(":{command}█"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]
    } else if state.input_mode == InputMode::Search {
        vec![Line::styled(
            format!("/{}█", state.search),
            Style::default().fg(Color::Yellow),
        )]
    } else if lines.len() == 1 && lines[0].is_empty() {
        vec![Line::styled(
            if state.side_active {
                "Ask a SIDE question… (/main returns to the main conversation)"
            } else if state.side_starting {
                "SIDE is starting… messages entered now remain isolated from MAIN"
            } else {
                "Type a message… (/side starts an isolated ephemeral conversation)"
            },
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        lines
            .iter()
            .skip(state.compose_scroll)
            .take(visible_rows)
            .map(|line| Line::raw(line.clone()))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(display)
            .block(
                Block::default()
                    .title(title)
                    .title_style(
                        Style::default()
                            .fg(border_color)
                            .add_modifier(Modifier::BOLD),
                    )
                    .border_style(Style::default().fg(border_color))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
    if state.input_mode == InputMode::Compose {
        let y = area.y
            + 1
            + cursor_row
                .saturating_sub(state.compose_scroll)
                .min(visible_rows.saturating_sub(1)) as u16;
        let x = area.x + 1 + cursor_col.min(inner_width.saturating_sub(1)) as u16;
        frame.set_cursor_position((x, y));
    }
}

fn wrapped_editor_lines(text: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let cursor = floor_grapheme_boundary(text, cursor);
    let mut lines = vec![String::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    for (byte, grapheme) in grapheme_indices(text) {
        if byte == cursor {
            cursor_row = row;
            cursor_col = col;
        }
        if grapheme == "\n" {
            lines.push(String::new());
            row += 1;
            col = 0;
            continue;
        }
        let rendered = if grapheme == "\t" { " " } else { grapheme };
        let grapheme_width = cell_width(rendered);
        if col > 0 && col + grapheme_width > width {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        lines[row].push_str(rendered);
        col += grapheme_width;
    }
    if cursor == text.len() {
        cursor_row = row;
        cursor_col = col;
    }
    (lines, cursor_row, cursor_col)
}

fn terminal_text_tail(text: &str, width: usize) -> String {
    if cell_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let available = width.saturating_sub(1);
    let graphemes = grapheme_indices(text)
        .map(|(_, grapheme)| grapheme)
        .collect::<Vec<_>>();
    let mut used = 0usize;
    let mut start = graphemes.len();
    for (index, grapheme) in graphemes.iter().enumerate().rev() {
        let grapheme_width = cell_width(grapheme);
        if used.saturating_add(grapheme_width) > available {
            break;
        }
        used = used.saturating_add(grapheme_width);
        start = index;
    }
    format!("…{}", graphemes[start..].concat())
}

fn visible_lane(state: &AppState) -> &'static str {
    if state.side_active {
        "SIDE"
    } else if state.side_starting {
        "SIDE STARTING"
    } else {
        "MAIN"
    }
}

fn render_agent_progress(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let progress = &state.agent_progress;
    let lane = visible_lane(state);
    let age = progress.last_event_age();
    let active = progress.phase.is_active();
    let quiet = active && age >= std::time::Duration::from_secs(5);
    let warning = active && age >= std::time::Duration::from_secs(15);
    let spinner = ["◐", "◓", "◑", "◒"][(progress.elapsed().as_millis() as usize / 200) % 4];
    let state_marker = if warning {
        "⚠"
    } else if quiet {
        "◇"
    } else if active {
        spinner
    } else if state.agent_connected {
        "●"
    } else {
        "○"
    };
    let color = if warning {
        Color::Red
    } else if quiet {
        Color::Yellow
    } else if active {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let compact = area.width < 60;
    let headline = if compact && area.height <= 3 {
        format!(
            " COPILOT {lane} {} · {}",
            progress.phase.label(),
            progress.summary
        )
    } else if compact {
        format!(
            " COPILOT {lane} {state_marker} {} · q{}",
            progress.phase.label(),
            progress.queue_depth,
        )
    } else if area.width < 90 {
        format!(
            " COPILOT {lane} {state_marker} {} {} · SDK {} · event #{} · q{}",
            progress.phase.label(),
            format_duration(progress.elapsed()),
            format_duration(age),
            progress.event_count,
            progress.queue_depth,
        )
    } else {
        format!(
            " COPILOT {lane} {state_marker} {} {} · last SDK event {} · event #{} · queue {} ",
            progress.phase.label(),
            format_duration(progress.elapsed()),
            format_duration(age),
            progress.event_count,
            progress.queue_depth,
        )
    };
    let mut lines = vec![Line::from(vec![Span::styled(
        fit_terminal_text(&headline, area.width.saturating_sub(2) as usize),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )])];
    if area.height == 1 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                fit_terminal_text(&headline, area.width as usize),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
            area,
        );
        return;
    }
    if area.height > 1 {
        lines.push(Line::styled(
            if warning {
                format!(
                    " :agent-status INSPECT · no SDK events for {} · connected={} · Ctrl-C stop · {}",
                    format_duration(age),
                    state.agent_connected,
                    progress.summary,
                )
            } else if quiet {
                format!(
                    " :agent-status INSPECT · quiet for {} · connected={} · {}",
                    format_duration(age),
                    state.agent_connected,
                    progress.summary,
                )
            } else {
                format!(" {} — {}", progress.summary, progress.detail)
            },
            Style::default().fg(color),
        ));
    }
    if compact && area.height > 3 {
        lines.push(Line::styled(
            format!(" detail: {}", progress.detail),
            Style::default().fg(color),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::TOP | Borders::BOTTOM))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_agent_status(frame: &mut ratatui::Frame, state: &AppState) {
    let area = frame.area();
    let block = Block::default()
        .title(" Agent status · explicit stuck diagnostics ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let footer_height = 1.min(inner.height);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(footer_height),
    );
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(footer_height),
        inner.width,
        footer_height,
    );
    let progress = &state.agent_progress;
    let lane = visible_lane(state);
    let compact = area.width < 60 || area.height < 16;
    let fixed_rows = if compact { 5 } else { 8 };
    let visible_timeline_rows = (body.height as usize).saturating_sub(fixed_rows);
    let timeline_total = progress.timeline.len();
    let timeline_start = if timeline_total == 0 {
        0
    } else {
        state.scroll.min(timeline_total.saturating_sub(1)) + 1
    };
    let timeline_end = if timeline_total == 0 {
        0
    } else {
        state
            .scroll
            .saturating_add(visible_timeline_rows.max(1))
            .min(timeline_total)
    };
    let timeline_position = format!("{timeline_start}-{timeline_end}/{timeline_total}");
    let fit = |text: String| {
        Line::raw(fit_terminal_text(
            &text,
            inner.width.saturating_sub(1) as usize,
        ))
    };
    let mut lines = if compact {
        vec![
            Line::styled(
                fit_terminal_text(
                    &format!(
                        "COPILOT {lane} · {} · q{}",
                        progress.phase.label(),
                        progress.queue_depth
                    ),
                    inner.width.saturating_sub(1) as usize,
                ),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            fit(format!(
                "elapsed {} · SDK {} ago · event #{} · connected={}",
                format_duration(progress.elapsed()),
                format_duration(progress.last_event_age()),
                progress.event_count,
                state.agent_connected
            )),
            fit(format!(
                "working: {} — {}",
                progress.summary, progress.detail
            )),
            fit(format!(
                "outbound: {}",
                progress
                    .active_outbound_id
                    .as_deref()
                    .map(short_id)
                    .unwrap_or("none")
            )),
            Line::styled(
                fit_terminal_text(
                    &format!("Recent SDK activity · {timeline_position}"),
                    inner.width.saturating_sub(1) as usize,
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]
    } else {
        vec![
            Line::styled(
                "COPILOT SDK LIVENESS",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(format!(
                "lane: {lane} · connected: {} · phase: {}",
                state.agent_connected,
                progress.phase.label()
            )),
            Line::raw(format!(
                "active elapsed: {} · last SDK event: {} ago · events: {} · queue: {}",
                format_duration(progress.elapsed()),
                format_duration(progress.last_event_age()),
                progress.event_count,
                progress.queue_depth
            )),
            Line::raw(format!("working on: {}", progress.summary)),
            Line::raw(format!("detail: {}", progress.detail)),
            Line::raw(format!(
                "outbound id: {}",
                progress.active_outbound_id.as_deref().unwrap_or("none")
            )),
            Line::from(""),
            Line::styled(
                format!("Recent SDK activity · {timeline_position} (newest last)"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]
    };
    lines.extend(progress.timeline.iter().skip(state.scroll).map(|entry| {
        let text = format!(
            "{:>7} ago · {}",
            format_duration(progress.timeline_age(entry.at)),
            entry.label
        );
        if compact {
            fit(text)
        } else {
            Line::raw(text)
        }
    }));
    let paragraph = Paragraph::new(lines);
    if compact {
        frame.render_widget(paragraph, body);
    } else {
        frame.render_widget(paragraph.wrap(Wrap { trim: false }), body);
    }
    let controls = if area.width < 60 {
        "j/k scroll · s/C-c stop · q/Esc back"
    } else {
        "q/Esc return · j/k or wheel · PgUp/PgDn · g/G · s/Ctrl-C stop"
    };
    frame.render_widget(
        Paragraph::new(controls).style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn render_queue(frame: &mut ratatui::Frame, state: &AppState) {
    let visible_width = frame.area().width.saturating_sub(40).max(8) as usize;
    let main_entries = state.main_chat.as_ref().into_iter().flatten().chain(
        (state.main_chat.is_none())
            .then_some(&state.chat)
            .into_iter()
            .flatten(),
    );
    let side_entries = if state.main_chat.is_some() {
        state.chat.iter().chain(state.pending_side_entries.iter())
    } else {
        [].iter().chain(state.pending_side_entries.iter())
    };
    let entries = main_entries
        .chain(side_entries)
        .filter(|entry| {
            entry.role != "copilot"
                && entry
                    .outbound_id
                    .as_ref()
                    .is_some_and(|id| state.pending_outbound_ids.contains(id))
        })
        .enumerate()
        .map(|(index, entry)| {
            let id = entry.outbound_id.as_deref().unwrap_or("unknown");
            let lane = if state.side_outbound_ids.contains(id) {
                "SIDE"
            } else {
                "MAIN"
            };
            let status = if state.agent_progress.active_outbound_id.as_deref() == Some(id)
                && state.agent_progress.phase.is_active()
            {
                "ACTIVE"
            } else {
                "QUEUED"
            };
            let text = entry.text.replace('\n', " ↵ ");
            let preview = text.chars().take(visible_width).collect::<String>();
            let marker = if index == state.scroll { "▶" } else { " " };
            ListItem::new(format!(
                "{marker} #{:<3} {status:<6} {lane:<4} {}  {preview}",
                index + 1,
                short_id(id)
            ))
            .style(if index == state.scroll {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    let total = entries.len();
    let viewport = frame.area().height.saturating_sub(4).max(1) as usize;
    let start = state.scroll.min(total.saturating_sub(viewport));
    let visible = entries
        .into_iter()
        .skip(start)
        .take(viewport)
        .collect::<Vec<_>>();
    let title = format!(
        " Copilot queue · {} pending · background FIFO ",
        state.pending_outbound_ids.len()
    );
    let list = if visible.is_empty() {
        List::new(vec![ListItem::new(
            "No active or queued questions. New Chat messages remain editable while Copilot works.",
        )])
    } else {
        List::new(visible)
    };
    frame.render_widget(
        list.block(Block::default().title(title).borders(Borders::ALL)),
        frame.area(),
    );
    let area = frame.area();
    let controls = if area.width < 50 {
        "j/k e edit · d cancel · s stop/C-c · q"
    } else if area.width < 80 {
        "j/k/Pg/g/G · wheel · e edit · d cancel · s/C-c stop · q/Esc"
    } else {
        "j/k/Pg/g/G or wheel · e edit · d cancel · s/Ctrl-C stop · q/Esc"
    };
    frame.render_widget(
        Paragraph::new(controls).style(Style::default().fg(Color::DarkGray)),
        Rect::new(
            area.x.saturating_add(1),
            area.bottom().saturating_sub(2),
            area.width.saturating_sub(2),
            1,
        ),
    );
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    } else if seconds > 0 {
        format!("{seconds}s")
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn agent_event_outbound_id(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::Queued { outbound_id, .. }
        | AgentEvent::QueueCancelled { outbound_id }
        | AgentEvent::QueueReplaced { outbound_id, .. }
        | AgentEvent::QueueReplaceRejected { outbound_id, .. }
        | AgentEvent::ResponseStarted { outbound_id, .. }
        | AgentEvent::ResponseDelta { outbound_id, .. }
        | AgentEvent::ResponseSnapshot { outbound_id, .. }
        | AgentEvent::ResponseComplete { outbound_id, .. }
        | AgentEvent::TurnFailed { outbound_id, .. } => Some(outbound_id.clone()),
        AgentEvent::SteeringAccepted {
            active_outbound_id, ..
        }
        | AgentEvent::SteeringFailed {
            active_outbound_id, ..
        } => Some(active_outbound_id.clone()),
        AgentEvent::Activity { outbound_id, .. } => outbound_id.clone(),
        _ => None,
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn unified_line(
    path: &std::path::Path,
    line: &DiffLine,
    index: usize,
    selected: bool,
    highlighter: &mut dyn Highlighter,
) -> Line<'static> {
    let marker = match line.kind {
        LineKind::Addition => "+",
        LineKind::Deletion => "-",
        LineKind::Context => " ",
        LineKind::Meta => "·",
    };
    let number = line.new_line.or(line.old_line).unwrap_or(0);
    let mut spans = vec![Span::styled(
        format!(
            "{}{:>5} {} ",
            if selected { "▶" } else { " " },
            number,
            marker
        ),
        gutter_style(line.kind),
    )];
    spans.extend(highlight_spans(
        highlighter
            .highlight_line(path, index, &line.content)
            .unwrap_or_else(|_| plain_segments(&line.content)),
    ));
    let mut result = Line::from(spans);
    if selected {
        result = result.style(Style::default().bg(Color::Rgb(40, 50, 65)));
    }
    result
}

fn split_line(
    line: Option<&DiffLine>,
    path: Option<&std::path::Path>,
    selection: Option<ReviewRowSelection>,
    highlighter: &mut dyn Highlighter,
) -> Line<'static> {
    let Some(line) = line else {
        return Line::from("");
    };
    let selected = selection.is_some();
    let number = line.new_line.or(line.old_line).unwrap_or(0);
    let mut spans = vec![Span::styled(
        format!("{}{:>4} ", if selected { "▶" } else { " " }, number),
        gutter_style(line.kind),
    )];
    spans.extend(highlight_spans(
        path.and_then(|path| highlighter.highlight_line(path, number, &line.content).ok())
            .unwrap_or_else(|| plain_segments(&line.content)),
    ));
    let mut rendered = Line::from(spans);
    if matches!(selection, Some(ReviewRowSelection::Whole)) {
        rendered = rendered.style(Style::default().bg(Color::Rgb(40, 50, 65)));
    }
    if let Some(ReviewRowSelection::Columns { start, end }) = selection {
        paint_content_columns(&mut rendered, 1, start, end);
    }
    rendered
}

fn gutter_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Addition => Style::default().fg(Color::Green),
        LineKind::Deletion => Style::default().fg(Color::Red),
        LineKind::Context => Style::default().fg(Color::DarkGray),
        LineKind::Meta => Style::default().fg(Color::Blue),
    }
}

fn highlight_spans(segments: Vec<StyledSegment>) -> Vec<Span<'static>> {
    segments
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

fn plain_segments(text: &str) -> Vec<StyledSegment> {
    vec![StyledSegment {
        text: text.to_owned(),
        foreground: (210, 210, 210),
        bold: false,
        italic: false,
    }]
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use anyhow::Result;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::{
        copy_to_clipboard_with_writer, finish_ready_prune, handle_agent_envelope,
        handle_agent_event, handle_effect, handle_effect_failure, load_model_preferences,
        load_ui_preferences, markdown_to_html, mouse_scroll_effects, open_browser_preview,
        parse_review_context, render, run_clipboard_candidate, table_cells,
    };
    use crate::app::{
        tests_support::state_for_ui, AgentPhase, ChatEntry, ComposeTarget, DiffLayout, Effect,
        Focus, InputMode, MarkdownPreview, PendingPrune, PruneChoice, ReadyPrune, Screen,
    };
    use crate::config::AppPaths;
    use crate::copilot::{
        AgentCommand, AgentEvent, AgentEventEnvelope, AgentLane, AgentSink, HistoryEntry,
        LaneEvent, ModelSelection, OutboundKind, PruneSessionOutcome,
    };
    use crate::diff::{DiffLine, LineKind};
    use crate::domain::{PendingChat, WorkItem};
    use crate::highlight::{Highlighter, PlainHighlighter, StyledSegment};
    use crate::storage::{now, Storage};

    #[derive(Default)]
    struct FakeAgent {
        commands: Mutex<Vec<AgentCommand>>,
    }

    impl AgentSink for FakeAgent {
        fn send(&self, command: AgentCommand) -> Result<()> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }
    }

    struct RejectingAgent;

    impl AgentSink for RejectingAgent {
        fn send(&self, _command: AgentCommand) -> Result<()> {
            anyhow::bail!("controlled send failure")
        }
    }

    #[derive(Default)]
    struct CountingHighlighter {
        calls: usize,
    }

    impl Highlighter for CountingHighlighter {
        fn highlight_line(
            &mut self,
            _path: &Path,
            _line_number: usize,
            text: &str,
        ) -> Result<Vec<StyledSegment>> {
            self.calls += 1;
            Ok(vec![StyledSegment {
                text: text.to_owned(),
                foreground: (210, 210, 210),
                bold: false,
                italic: false,
            }])
        }
    }

    fn paths() -> AppPaths {
        AppPaths {
            data: PathBuf::from("/tmp/rq-tui-test/data"),
            cache: PathBuf::from("/tmp/rq-tui-test/cache"),
            database: PathBuf::from("/tmp/rq-tui-test/data/db"),
            roots: PathBuf::from("/tmp/rq-tui-test/data/roots"),
            prs: PathBuf::from("/tmp/rq-tui-test/cache/prs"),
            exports: PathBuf::from("/tmp/rq-tui-test/data/exports"),
            skills: PathBuf::from("/tmp/rq-tui-test/data/skills"),
            plugins: PathBuf::from("/tmp/rq-tui-test/data/plugins"),
        }
    }

    #[test]
    fn review_screen_renders_deterministically_without_a_real_terminal() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = state_for_ui();
        let mut highlighter = PlainHighlighter;
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("demo — Review"));
        assert!(content.contains("unified"));
        assert!(content.contains("a ask"));
    }

    #[test]
    fn settings_preferences_persist_and_restore_launch_behavior() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();

        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::SetDefaultDiffLayout(DiffLayout::Split),
        )
        .unwrap();
        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::SetFileTreeDefault(false),
        )
        .unwrap();
        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::SetMarkdownPreview(MarkdownPreview::Browser),
        )
        .unwrap();

        let mut restarted = state_for_ui();
        load_ui_preferences(&mut restarted, &storage, &paths()).unwrap();
        assert_eq!(restarted.layout, DiffLayout::Split);
        assert_eq!(restarted.default_layout, DiffLayout::Split);
        assert!(!restarted.picker_open);
        assert!(!restarted.file_tree_default_open);
        assert_eq!(restarted.markdown_preview, MarkdownPreview::Browser);
        assert_eq!(restarted.cache_directory, "/tmp/rq-tui-test/cache/prs");
        assert_eq!(restarted.storage_path, "/tmp/rq-tui-test/data/db");
    }

    #[test]
    fn model_preferences_are_scoped_to_each_work_item() {
        let storage = Storage::in_memory().unwrap();
        let mut first = state_for_ui();
        first.work_item.item.id = "first-work-item".into();
        handle_agent_event(
            &mut first,
            &storage,
            AgentEvent::ModelSelectionChanged(ModelSelection {
                model_id: "deep".into(),
                reasoning_effort: Some("high".into()),
                context_tier: Some("long_context".into()),
            }),
        )
        .unwrap();

        let mut second = state_for_ui();
        second.work_item.item.id = "second-work-item".into();
        handle_agent_event(
            &mut second,
            &storage,
            AgentEvent::ModelSelectionChanged(ModelSelection {
                model_id: "fast".into(),
                reasoning_effort: Some("low".into()),
                context_tier: Some("default".into()),
            }),
        )
        .unwrap();

        let mut restarted_first = state_for_ui();
        restarted_first.work_item.item.id = "first-work-item".into();
        load_model_preferences(&mut restarted_first, &storage).unwrap();
        assert_eq!(restarted_first.model, "deep");
        assert_eq!(restarted_first.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            restarted_first.context_tier.as_deref(),
            Some("long_context")
        );

        let mut restarted_second = state_for_ui();
        restarted_second.work_item.item.id = "second-work-item".into();
        load_model_preferences(&mut restarted_second, &storage).unwrap();
        assert_eq!(restarted_second.model, "fast");
        assert_eq!(restarted_second.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(restarted_second.context_tier.as_deref(), Some("default"));
    }

    #[test]
    fn rejected_model_selection_retains_the_active_configuration_and_is_retryable() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.model = "current-model".into();
        state.reasoning_effort = Some("medium".into());
        state.context_tier = Some("default".into());

        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::ModelSelectionFailed {
                selection: ModelSelection {
                    model_id: "rejected-model".into(),
                    reasoning_effort: Some("high".into()),
                    context_tier: Some("long_context".into()),
                },
                message: "runtime refused this model".into(),
            },
        )
        .unwrap();

        assert_eq!(state.model, "current-model");
        assert_eq!(state.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(state.context_tier.as_deref(), Some("default"));
        assert_eq!(state.agent_progress.phase, AgentPhase::Failed);
        assert!(state.status.contains("still using current-model"));
        assert!(state.status.contains("retry with :model"));
    }

    #[test]
    fn rejected_immediate_steering_settles_its_durable_record() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();
        storage.upsert_work_item(&state.work_item.item).unwrap();
        state.agent_progress.phase = AgentPhase::Responding;
        state.agent_progress.active_outbound_id = Some("active-turn".into());

        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::SteerChat("focus on cleanup".into()),
        )
        .unwrap();
        let steering_id = match agent.commands.lock().unwrap().last().cloned().unwrap() {
            AgentCommand::Steer(outbound) => outbound.id,
            command => panic!("expected steering command, got {command:?}"),
        };
        assert_eq!(state.pending_outbound_ids.len(), 1);
        assert_eq!(
            storage
                .pending_chats(&state.work_item.item.id)
                .unwrap()
                .len(),
            1
        );

        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::SteeringFailed {
                steering_id: steering_id.clone(),
                active_outbound_id: "active-turn".into(),
                message: "runtime rejected immediate delivery".into(),
            },
        )
        .unwrap();

        assert!(state.pending_outbound_ids.is_empty());
        assert!(storage
            .pending_chats(&state.work_item.item.id)
            .unwrap()
            .is_empty());
        assert_eq!(state.agent_progress.phase, AgentPhase::Responding);
        assert_eq!(
            state.agent_progress.active_outbound_id.as_deref(),
            Some("active-turn")
        );
        assert!(state
            .chat
            .iter()
            .find(|entry| entry.outbound_id.as_deref() == Some(steering_id.as_str()))
            .and_then(|entry| entry.error.as_deref())
            .is_some_and(|error| error.contains("steering failed")));
    }

    #[test]
    fn abort_delivery_failure_never_leaves_a_false_stopping_state() {
        let mut state = state_for_ui();
        state.agent_progress.phase = AgentPhase::Responding;
        state.agent_progress.active_outbound_id = Some("active-turn".into());
        let storage = Storage::in_memory().unwrap();
        let effect = Effect::AbortAgent;
        let error = handle_effect(
            &mut state,
            &storage,
            &paths(),
            &RejectingAgent,
            effect.clone(),
        )
        .unwrap_err();
        handle_effect_failure(&mut state, &effect, &error);

        assert_eq!(state.agent_progress.phase, AgentPhase::Responding);
        assert_eq!(
            state.agent_progress.active_outbound_id.as_deref(),
            Some("active-turn")
        );
        assert!(state.status.contains("retry with :stop"));
        assert!(state.agent_progress.detail.contains("may still be active"));

        state.input_mode = InputMode::Command;
        for character in "stop".chars() {
            state.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            vec![Effect::AbortAgent]
        );
        assert_eq!(state.agent_progress.phase, AgentPhase::Stopping);
    }

    #[test]
    fn stop_acknowledgement_after_sdk_idle_cannot_leave_the_ui_stopping() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.agent_progress.phase = AgentPhase::Stopping;
        state.agent_progress.active_outbound_id = Some("settled-turn".into());
        state.pending_outbound_ids.insert("settled-turn".into());
        state.chat.push(ChatEntry {
            id: "settled-response".into(),
            role: "assistant".into(),
            text: "finished before cancellation arrived".into(),
            outbound_id: Some("settled-turn".into()),
            streaming: true,
            annotation_id: None,
            error: None,
        });

        handle_agent_event(&mut state, &storage, AgentEvent::StopSettledAlreadyIdle).unwrap();

        assert_eq!(state.agent_progress.phase, AgentPhase::Idle);
        assert_eq!(state.agent_progress.active_outbound_id, None);
        assert!(!state.pending_outbound_ids.contains("settled-turn"));
        assert!(!state.chat.last().unwrap().streaming);
        assert!(state.status.contains("nothing is stuck"));
    }

    #[test]
    fn late_steering_results_settle_without_rewinding_the_current_turn() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        storage.upsert_work_item(&state.work_item.item).unwrap();
        state.agent_progress.phase = AgentPhase::Responding;
        state.agent_progress.active_outbound_id = Some("new-turn".into());

        for (steering_id, accepted) in [("late-accepted", true), ("late-failed", false)] {
            state.pending_outbound_ids.insert(steering_id.into());
            storage
                .enqueue_chat(&PendingChat {
                    id: steering_id.into(),
                    work_item_id: state.work_item.item.id.clone(),
                    text: "late correction".into(),
                    kind: "correction".into(),
                    lane: "main".into(),
                    created_at: now(),
                })
                .unwrap();
            let event = if accepted {
                AgentEvent::SteeringAccepted {
                    steering_id: steering_id.into(),
                    active_outbound_id: "old-turn".into(),
                }
            } else {
                AgentEvent::SteeringFailed {
                    steering_id: steering_id.into(),
                    active_outbound_id: "old-turn".into(),
                    message: "late rejection".into(),
                }
            };
            handle_agent_event(&mut state, &storage, event).unwrap();

            assert_eq!(state.agent_progress.phase, AgentPhase::Responding);
            assert_eq!(
                state.agent_progress.active_outbound_id.as_deref(),
                Some("new-turn")
            );
            assert!(!state.pending_outbound_ids.contains(steering_id));
        }
        assert!(storage
            .pending_chats(&state.work_item.item.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn resumed_session_history_failure_remains_a_visible_warning() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        storage.upsert_work_item(&state.work_item.item).unwrap();
        let warning =
            "Session resumed, but its conversation history could not be restored: timed out";
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::SessionReady {
                session_id: "resumed-session".into(),
                resumed: true,
                resume_warning: Some(warning.into()),
            },
        )
        .unwrap();

        assert_eq!(state.status, warning);
        assert_eq!(state.agent_progress.detail, warning);
        assert!(state.agent_connected);
        assert!(state.chat.is_empty());
    }

    #[test]
    fn settings_panel_is_truthful_and_fully_navigable() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = state_for_ui();
        state.screen = Screen::Settings;
        state.settings_index = 9;
        load_ui_preferences(&mut state, &Storage::in_memory().unwrap(), &paths()).unwrap();
        let mut highlighter = PlainHighlighter;
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Diff layout       unified (launch default)"));
        assert!(content.contains("File tree default open"));
        assert!(content.contains("❯ Markdown preview  inline"));
        assert!(content.contains("Cache dir"));
        assert!(content.contains("Skills dir"));
        assert!(!content.contains("Markdown preview  system browser"));

        let compact_backend = TestBackend::new(72, 9);
        let mut compact_terminal = Terminal::new(compact_backend).unwrap();
        state.settings_index = 11;
        state.storage_path =
            "/a/very/long/storage/path/that/cannot/fit/in/the/minimum/settings/viewport/review.db"
                .into();
        compact_terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        let compact = compact_terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(compact.contains("Settings 6-12/12"));
        assert!(compact.contains("❯ Storage"));
        assert!(compact.contains('…'));
    }

    #[test]
    fn orphan_side_cleanup_is_explicit_without_changing_main_state() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.agent_connected = true;
        state.chat = vec![ChatEntry {
            id: "main".into(),
            role: "you".into(),
            text: "keep MAIN".into(),
            streaming: false,
            annotation_id: None,
            outbound_id: None,
            error: None,
        }];

        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::OrphanSideCleanup {
                session_id: "side-session".into(),
                cleanup_warning: None,
            },
        )
        .unwrap();
        assert!(state.status.contains("side-ses"));
        assert!(state.status.contains("cleaned"));
        assert_eq!(state.chat[0].text, "keep MAIN");
        assert!(state.agent_connected);

        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::OrphanSideCleanup {
                session_id: "side-session".into(),
                cleanup_warning: Some("retry pending".into()),
            },
        )
        .unwrap();
        assert!(state.status.contains("retry pending"));
        assert_eq!(state.chat[0].text, "keep MAIN");
        assert!(state.agent_connected);

        handle_agent_event(&mut state, &storage, AgentEvent::Stopped).unwrap();
        assert!(state.status.contains("retry pending"));
        assert!(state.agent_progress.detail.contains("retry pending"));
    }

    #[test]
    fn hidden_main_stream_updates_parked_transcript_without_moving_side() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.side_active = true;
        state.side_session_id = Some("side-session".into());
        state.main_chat = Some(vec![ChatEntry {
            id: "main-user".into(),
            role: "you".into(),
            text: "main prompt".into(),
            streaming: false,
            annotation_id: None,
            outbound_id: Some("main-outbound".into()),
            error: None,
        }]);
        state.chat = vec![ChatEntry {
            id: "side-user".into(),
            role: "you".into(),
            text: "side prompt".into(),
            streaming: false,
            annotation_id: None,
            outbound_id: Some("side-outbound".into()),
            error: None,
        }];
        state.chat_scroll = 7;
        state.chat_autofollow = false;
        state.status = "SIDE remains visible".into();

        handle_agent_envelope(
            &mut state,
            &storage,
            AgentEventEnvelope {
                lane: AgentLane::Main,
                event: LaneEvent::Agent(AgentEvent::ResponseStarted {
                    outbound_id: "main-outbound".into(),
                    outbound: OutboundKind::Chat,
                    first_delta: "background MAIN response".into(),
                }),
                activity: None,
            },
        )
        .unwrap();

        assert_eq!(state.chat.len(), 1);
        assert_eq!(state.chat[0].text, "side prompt");
        assert_eq!(state.chat_scroll, 7);
        assert!(!state.chat_autofollow);
        assert_eq!(state.status, "SIDE remains visible");
        assert!(state
            .main_chat
            .as_ref()
            .unwrap()
            .iter()
            .any(|entry| entry.text == "background MAIN response"));
    }

    #[test]
    fn fatal_side_events_restore_main_and_settle_its_transcript() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.agent_connected = true;
        state.side_active = true;
        state.side_session_id = Some("side-session".into());
        state.main_chat = Some(vec![ChatEntry {
            id: "main-stream".into(),
            role: "copilot".into(),
            text: "main partial".into(),
            streaming: true,
            annotation_id: None,
            outbound_id: Some("main-outbound".into()),
            error: None,
        }]);
        state.chat = vec![ChatEntry {
            id: "side-stream".into(),
            role: "copilot".into(),
            text: "side partial".into(),
            streaming: true,
            annotation_id: None,
            outbound_id: Some("side-outbound".into()),
            error: None,
        }];

        handle_agent_envelope(
            &mut state,
            &storage,
            AgentEventEnvelope {
                lane: AgentLane::Side {
                    id: "side-session".into(),
                },
                event: LaneEvent::Agent(AgentEvent::Error("event stream closed".into())),
                activity: None,
            },
        )
        .unwrap();

        assert!(!state.agent_connected);
        assert!(!state.side_active);
        assert!(state.main_chat.is_none());
        assert_eq!(state.chat[0].text, "main partial");
        assert!(state.status.contains("event stream closed"));
        assert_eq!(state.chat[0].error.as_deref(), Some("connection lost"));

        state.agent_connected = true;
        state.side_active = true;
        state.side_session_id = Some("side-session".into());
        state.main_chat = Some(std::mem::take(&mut state.chat));
        state.chat.push(ChatEntry {
            id: "side-stream-2".into(),
            role: "copilot".into(),
            text: "another side partial".into(),
            streaming: true,
            annotation_id: None,
            outbound_id: Some("side-outbound-2".into()),
            error: None,
        });
        state.main_chat.as_mut().unwrap()[0].streaming = true;
        state.main_chat.as_mut().unwrap()[0].error = None;
        handle_agent_envelope(
            &mut state,
            &storage,
            AgentEventEnvelope {
                lane: AgentLane::Side {
                    id: "side-session".into(),
                },
                event: LaneEvent::Agent(AgentEvent::Stopped),
                activity: None,
            },
        )
        .unwrap();
        assert!(!state.side_active);
        assert!(state.main_chat.is_none());
        assert_eq!(state.chat[0].error.as_deref(), Some("worker stopped"));

        state.side_starting = true;
        state.pending_side_entries.push(ChatEntry {
            id: "pending-side".into(),
            role: "you".into(),
            text: "never opened".into(),
            streaming: false,
            annotation_id: None,
            outbound_id: Some("pending-side-outbound".into()),
            error: None,
        });
        handle_agent_envelope(
            &mut state,
            &storage,
            AgentEventEnvelope {
                lane: AgentLane::Main,
                event: LaneEvent::Agent(AgentEvent::Error("startup failed".into())),
                activity: None,
            },
        )
        .unwrap();
        assert!(!state.side_starting);
        assert!(state.pending_side_entries.is_empty());
        assert!(state.status.contains("MAIN restored"));
    }

    #[test]
    fn failed_side_start_consumes_saved_main_viewport_before_restoring_draft() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.screen = Screen::Chat;
        state.focus = Focus::Chat;
        state.chat_scroll = 4;
        state.chat_autofollow = false;

        state.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        for character in "/side retry me".chars() {
            state.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let effects = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let effect = effects
            .into_iter()
            .find(|effect| matches!(effect, Effect::StartSide(_)))
            .expect("side effect");
        let error = handle_effect(
            &mut state,
            &storage,
            &paths(),
            &RejectingAgent,
            effect.clone(),
        )
        .expect_err("agent rejects SIDE start");
        handle_effect_failure(&mut state, &effect, &error);

        assert_eq!(state.compose, "/side retry me");
        assert_eq!(state.input_mode, crate::app::InputMode::Compose);
        state.focus = Focus::Diff;
        state.reset_chat_semantics();
        assert_eq!(
            state.focus,
            Focus::Diff,
            "a stale MAIN snapshot must not restore during a later layout reset"
        );
    }

    #[test]
    fn hung_clipboard_backend_is_killed_within_the_timeout() {
        let started = std::time::Instant::now();
        let copied = run_clipboard_candidate(
            "sh",
            &["-c", "sleep 5"],
            &"x".repeat(1_000_000),
            std::time::Duration::from_millis(50),
        );
        assert!(!copied);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "clipboard timeout must not stall the TUI"
        );
    }

    #[test]
    fn large_review_highlights_only_the_visible_viewport() {
        let mut state = state_for_ui();
        state.work_item.repos[0].diff.files[0].hunks[0].lines = (0..12_000)
            .map(|index| DiffLine {
                kind: LineKind::Addition,
                old_line: None,
                new_line: Some(index + 1),
                content: format!("let generated_{index} = {index};"),
            })
            .collect();

        for layout in [DiffLayout::Unified, DiffLayout::Split] {
            state.layout = layout;
            let backend = TestBackend::new(100, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut highlighter = CountingHighlighter::default();
            terminal
                .draw(|frame| render(frame, &mut state, &mut highlighter))
                .unwrap();
            assert!(
                highlighter.calls <= 30,
                "{layout:?} highlighted {} lines for a 30-row terminal",
                highlighter.calls
            );
            assert!(highlighter.calls > 0);
        }
    }

    #[test]
    fn total_clipboard_failure_is_injectable_and_visibly_reported() {
        struct RejectWrites;

        impl std::io::Write for RejectWrites {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("simulated OSC 52 write failure"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let error = copy_to_clipboard_with_writer("exact text", true, &mut RejectWrites)
            .expect_err("forced OSC 52 output must expose write failures");
        let mut state = state_for_ui();
        handle_effect_failure(&mut state, &Effect::Yank("exact text".into()), &error);
        assert!(state.status.contains("Action failed"));
        assert!(state.status.contains("simulated OSC 52 write failure"));
    }

    #[test]
    fn markdown_preview_renders_full_height_with_scroll_controls() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = state_for_ui();
        state.open_preview(
            "# Review\n\nA paragraph with **emphasis**.\n\n```rust\nfn main() {}\n```\n\n"
                .to_owned()
                + &(0..30)
                    .map(|index| format!("line {index}\n"))
                    .collect::<String>(),
        );
        let mut highlighter = PlainHighlighter;
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        let first = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(first.contains("Markdown preview"));
        assert!(first.contains("Review"));
        assert!(first.contains("C-u/d half"));
        assert!(state.preview_total_rows > 24);

        state.preview_scroll = 20;
        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        let second = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(second.contains("Markdown preview · rows "));
        assert!(state.preview_scroll > 0);
        assert!(state.preview_scroll < 20);
        assert!(second.contains("line 20"));
    }

    #[test]
    fn preview_effect_opens_the_in_tui_overlay_without_browser_side_effects() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();
        state.screen = Screen::Chat;
        state.focus = Focus::Chat;
        state.chat.push(ChatEntry {
            id: "chat-message".into(),
            role: "you".into(),
            text: "# In TUI\n\npreview me".into(),
            streaming: false,
            annotation_id: None,
            outbound_id: None,
            error: None,
        });
        handle_effect(&mut state, &storage, &paths(), &agent, Effect::Preview).unwrap();
        assert_eq!(state.screen, Screen::Preview);
        assert_eq!(state.focus, Focus::Chat);
        assert_eq!(
            state.preview_markdown.as_deref(),
            Some("# In TUI\n\npreview me")
        );
    }

    #[test]
    fn failed_browser_request_keeps_a_truthful_inline_fallback() {
        let mut state = state_for_ui();
        state.open_preview("# Browser fallback".into());

        open_browser_preview(&mut state, &paths(), "rq-tui-opener-that-does-not-exist");

        assert_eq!(state.screen, Screen::Preview);
        assert_eq!(
            state.preview_markdown.as_deref(),
            Some("# Browser fallback")
        );
        assert!(state.status.contains("Browser preview unavailable"));
        assert!(state.status.contains("inline preview remains open"));
    }

    #[test]
    fn side_effects_can_use_a_fake_agent_without_starting_copilot() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();
        storage.upsert_work_item(&state.work_item.item).unwrap();
        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::SendChat("hello".into()),
        )
        .unwrap();
        assert_eq!(state.chat[0].text, "hello");
        assert!(matches!(
            agent.commands.lock().unwrap().as_slice(),
            [AgentCommand::Send(_)]
        ));
        assert_eq!(state.pending_outbound_ids.len(), 1);
    }

    #[test]
    fn mouse_wheel_routes_through_the_active_command_or_composer_mode() {
        let mut state = state_for_ui();
        state.screen = Screen::Chat;
        state.focus = Focus::Chat;
        state.chat_scroll = 5;
        state.input_mode = InputMode::Command;
        state.command_viewport_rows = 2;

        assert!(mouse_scroll_effects(&mut state, MouseEventKind::ScrollDown).is_empty());
        assert_eq!(state.command_index, 3);
        assert_eq!(state.chat_scroll, 5);

        state.input_mode = InputMode::Compose;
        state.compose_target = Some(ComposeTarget::Chat);
        state.compose = "one\ntwo\nthree\nfour".into();
        state.compose_cursor = state.compose.len();
        state.compose_wrap_width = 40;
        let end = state.compose_cursor;

        assert!(mouse_scroll_effects(&mut state, MouseEventKind::ScrollUp).is_empty());
        assert!(state.compose_cursor < end);
        assert_eq!(state.chat_scroll, 5);
    }

    #[test]
    fn prune_rejects_the_open_work_item_without_agent_or_local_calls() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();
        storage.upsert_work_item(&state.work_item.item).unwrap();
        let current_id = state.work_item.item.id.clone();

        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::PruneWorkItems {
                ids: vec![current_id.clone()],
                export_first: false,
            },
        )
        .unwrap();

        assert!(agent.commands.lock().unwrap().is_empty());
        assert!(storage.work_item_by_id(&current_id).unwrap().is_some());
        assert!(state.pending_prune.is_none());
        assert!(state.status.contains("open Work Item cannot be pruned"));
    }

    #[test]
    fn prune_is_visibly_queued_until_the_worker_reports_that_cleanup_started() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();
        storage.upsert_work_item(&state.work_item.item).unwrap();
        storage
            .upsert_work_item(&WorkItem {
                id: "old-review".into(),
                name: "old review".into(),
                workspace_root: PathBuf::from("/old-review"),
                created_at: "1".into(),
                updated_at: "1".into(),
                last_opened_at: Some("1".into()),
            })
            .unwrap();

        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::PruneWorkItems {
                ids: vec!["old-review".into()],
                export_first: false,
            },
        )
        .unwrap();

        let request_id = state.pending_prune.as_ref().unwrap().request_id.clone();
        assert!(state.status.contains("Prune queued"));
        assert!(state
            .status
            .contains("waiting for the current Copilot turn"));
        assert_eq!(state.agent_progress.summary, "Prune queued");

        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::PruneSessionsStarted {
                request_id,
                work_items: 1,
            },
        )
        .unwrap();

        assert!(state.status.contains("cleanup started"));
        assert_eq!(state.agent_progress.summary, "Deleting Copilot sessions");
    }

    #[test]
    fn prune_completion_applies_each_work_item_independently() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();
        storage.upsert_work_item(&state.work_item.item).unwrap();
        for id in ["prune-ok", "prune-failed"] {
            storage
                .upsert_work_item(&WorkItem {
                    id: id.into(),
                    name: id.into(),
                    workspace_root: PathBuf::from(format!("/{id}")),
                    created_at: "1".into(),
                    updated_at: "1".into(),
                    last_opened_at: Some("1".into()),
                })
                .unwrap();
        }

        handle_effect(
            &mut state,
            &storage,
            &paths(),
            &agent,
            Effect::PruneWorkItems {
                ids: vec!["prune-ok".into(), "prune-failed".into()],
                export_first: false,
            },
        )
        .unwrap();
        let request_id = state.pending_prune.as_ref().unwrap().request_id.clone();
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::PruneSessionsComplete {
                request_id,
                outcomes: vec![
                    PruneSessionOutcome {
                        work_item_id: "prune-ok".into(),
                        remote_deleted: true,
                        local_deleted: true,
                        error: None,
                    },
                    PruneSessionOutcome {
                        work_item_id: "prune-failed".into(),
                        remote_deleted: false,
                        local_deleted: false,
                        error: Some("controlled SDK failure".into()),
                    },
                ],
            },
        )
        .unwrap();
        storage.delete_work_item("prune-ok").unwrap();
        finish_ready_prune(&mut state, &storage).unwrap();

        assert!(storage.work_item_by_id("prune-ok").unwrap().is_none());
        assert!(storage.work_item_by_id("prune-failed").unwrap().is_some());
        assert!(state.status.contains("Local history deleted 1"));
        assert!(state.status.contains("retained local history 1"));
        assert!(state.status.contains("controlled SDK failure"));
    }

    #[test]
    fn interrupted_prune_progress_is_visible_without_an_interactive_request() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();

        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::PruneRecoveryStarted {
                operation_id: "startup-recovery".into(),
                work_item_id: "old-review".into(),
            },
        )
        .unwrap();
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::PruneSessionProgress {
                request_id: "startup-recovery".into(),
                work_item_id: "old-review".into(),
                label: "Deleting Copilot session abc123".into(),
                completed: 2,
                total: 3,
            },
        )
        .unwrap();

        assert_eq!(state.agent_progress.summary, "Recovering interrupted prune");
        assert!(state.agent_progress.detail.contains("old-review"));
        assert!(state.agent_progress.detail.contains("2/3"));
        assert!(state.status.contains("Deleting Copilot session abc123"));
    }

    #[test]
    fn journal_warning_never_claims_already_deleted_local_history_was_retained() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.prune_items = vec![PruneChoice {
            id: "gone".into(),
            name: "gone".into(),
            last_opened_at: "1".into(),
            versions: 1,
            annotations: 0,
            selected: true,
        }];
        state.pending_prune = Some(PendingPrune {
            request_id: "request".into(),
            work_item_ids: vec!["gone".into()],
            skipped_current: 0,
        });
        state.ready_prune = Some(ReadyPrune {
            request_id: "request".into(),
            outcomes: vec![PruneSessionOutcome {
                work_item_id: "gone".into(),
                remote_deleted: true,
                local_deleted: true,
                error: Some(
                    "local cleanup completed, but its prune journal could not be cleared".into(),
                ),
            }],
        });

        finish_ready_prune(&mut state, &storage).unwrap();

        assert!(state.prune_items.is_empty());
        assert!(state.status.contains("local history deleted"));
        assert!(!state.status.contains("retained"));
        assert_eq!(
            state.agent_progress.summary,
            "Prune completed with a journal warning"
        );
    }

    #[test]
    fn mixed_prune_results_keep_both_failures_and_journal_warnings_visible() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        storage
            .upsert_work_item(&WorkItem {
                id: "retained".into(),
                name: "retained".into(),
                workspace_root: PathBuf::from("/retained"),
                created_at: "1".into(),
                updated_at: "1".into(),
                last_opened_at: Some("1".into()),
            })
            .unwrap();
        state.pending_prune = Some(PendingPrune {
            request_id: "request".into(),
            work_item_ids: vec!["gone".into(), "retained".into()],
            skipped_current: 0,
        });
        state.ready_prune = Some(ReadyPrune {
            request_id: "request".into(),
            outcomes: vec![
                PruneSessionOutcome {
                    work_item_id: "gone".into(),
                    remote_deleted: true,
                    local_deleted: true,
                    error: Some("completed journal could not be cleared".into()),
                },
                PruneSessionOutcome {
                    work_item_id: "retained".into(),
                    remote_deleted: false,
                    local_deleted: false,
                    error: Some("controlled remote failure".into()),
                },
            ],
        });

        finish_ready_prune(&mut state, &storage).unwrap();

        assert!(state
            .status
            .contains("completed journal could not be cleared"));
        assert!(state.status.contains("controlled remote failure"));
        assert!(state.status.contains("retained local history 1"));
        assert_eq!(state.agent_progress.phase, AgentPhase::Failed);
    }

    #[test]
    fn side_lane_cannot_inject_global_prune_recovery_events() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        let prior_summary = state.agent_progress.summary.clone();

        handle_agent_envelope(
            &mut state,
            &storage,
            AgentEventEnvelope {
                lane: AgentLane::Side { id: "side".into() },
                event: LaneEvent::Agent(AgentEvent::PruneRecoveryStarted {
                    operation_id: "stale".into(),
                    work_item_id: "victim".into(),
                }),
                activity: None,
            },
        )
        .unwrap();

        assert!(state.prune_recoveries.is_empty());
        assert_eq!(state.agent_progress.summary, prior_summary);
    }

    #[test]
    fn prune_render_labels_the_open_item_as_disabled() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = state_for_ui();
        state.screen = Screen::Prune;
        state.prune_items = vec![PruneChoice {
            id: state.work_item.item.id.clone(),
            name: "open review".into(),
            last_opened_at: "now".into(),
            versions: 1,
            annotations: 2,
            selected: false,
        }];
        let mut highlighter = PlainHighlighter;

        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(content.contains("open item disabled"));
        assert!(content.contains("OPEN(disabled)"));
        assert!(content.contains("Starting Copilot SDK session"));
    }

    #[test]
    fn prune_render_scrolls_to_selection_and_keeps_disabled_marker_at_narrow_width() {
        let backend = TestBackend::new(50, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = state_for_ui();
        state.screen = Screen::Prune;
        state.prune_items = (0..10)
            .map(|index| PruneChoice {
                id: if index == 9 {
                    state.work_item.item.id.clone()
                } else {
                    format!("item-{index}")
                },
                name: format!("item-{index}-with-a-name-that-is-far-too-long-for-the-terminal"),
                last_opened_at: "now".into(),
                versions: 1,
                annotations: 2,
                selected: false,
            })
            .collect();
        state.prune_index = 9;
        let mut highlighter = PlainHighlighter;

        terminal
            .draw(|frame| render(frame, &mut state, &mut highlighter))
            .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(content.contains("OPEN(disabled)"));
        assert!(content.contains("item-9"));
        assert!(!content.contains("item-0-with"));
    }

    #[test]
    fn resumed_history_and_streaming_updates_renderable_chat_state() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::HistoryLoaded(vec![
                HistoryEntry {
                    role: "you".into(),
                    text: "Earlier question".into(),
                },
                HistoryEntry {
                    role: "copilot".into(),
                    text: "Earlier answer".into(),
                },
            ]),
        )
        .unwrap();
        state.pending_outbound_ids.insert("outbound".into());
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::ResponseStarted {
                outbound_id: "outbound".into(),
                outbound: OutboundKind::Chat,
                first_delta: "New ".into(),
            },
        )
        .unwrap();
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::ResponseDelta {
                outbound_id: "outbound".into(),
                delta: "answer".into(),
            },
        )
        .unwrap();
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::ResponseComplete {
                outbound_id: "outbound".into(),
                aborted: false,
            },
        )
        .unwrap();

        assert_eq!(state.chat.len(), 3);
        assert_eq!(state.chat[2].text, "New answer");
        assert!(!state.chat[2].streaming);
        assert!(state.pending_outbound_ids.is_empty());
    }

    #[test]
    fn failed_turn_clears_spinner_and_keeps_chat_usable() {
        let storage = Storage::in_memory().unwrap();
        let mut state = state_for_ui();
        state.pending_outbound_ids.insert("outbound".into());
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::ResponseStarted {
                outbound_id: "outbound".into(),
                outbound: OutboundKind::Chat,
                first_delta: "Partial".into(),
            },
        )
        .unwrap();
        handle_agent_event(
            &mut state,
            &storage,
            AgentEvent::TurnFailed {
                outbound_id: "outbound".into(),
                outbound: OutboundKind::Chat,
                message: "network error".into(),
                response_started: true,
            },
        )
        .unwrap();

        assert!(!state.chat[0].streaming);
        assert_eq!(state.chat[0].error.as_deref(), Some("network error"));
        assert!(state.pending_outbound_ids.is_empty());
        assert!(!state.status.contains("unavailable"));
    }

    #[test]
    fn generated_context_parser_accepts_the_six_field_shape() {
        let context = parse_review_context(
            "work",
            "Title: Demo\nWhat: Change\nWhy: Safety\nHow: Checks\n\
             Considerations: Cost\nOther approaches: Cache",
        );
        assert_eq!(context.work_item_id, "work");
        assert_eq!(context.title, "Demo");
        assert_eq!(context.alternatives, "Cache");
    }

    #[test]
    fn markdown_preview_renders_links_tables_inline_markup_and_code_safely() {
        assert_eq!(table_cells(r"| x\|y | z |").unwrap().len(), 2);
        assert_eq!(table_cells(r"| x\\| y | z |").unwrap().len(), 3);

        let html = markdown_to_html(
            "# **Review**\n\n- [docs](https://example.invalid)\n\n\
             | Name | Result |\n| :--- | ---: |\n| `api` | <safe> |\n| a\\|b | exact |\n| C:\\temp | ok |\n\n\
             [bad](javascript:alert(1))\n\n```\n<a>\n```",
        );
        assert!(html.contains("<h1><strong>Review</strong></h1>"));
        assert!(html.contains("<a href=\"https://example.invalid\">docs</a>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<th style=\"text-align:left\">Name</th>"));
        assert!(html.contains("<td style=\"text-align:right\">&lt;safe&gt;</td>"));
        assert!(html.contains("<td style=\"text-align:left\">a|b</td>"));
        assert!(html.contains(r#"<td style="text-align:left">C:\temp</td>"#));
        assert!(html.contains("class=\"unsafe-link\""));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("&lt;a&gt;"));
    }
}

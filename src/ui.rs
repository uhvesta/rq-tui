use std::io;
use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use base64::Engine as _;
use crossterm::cursor::Show;
use crossterm::event::MouseEventKind;
use crossterm::event::{self, Event};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;

use crate::annotations::{
    anchor_from_diff, create_local_annotation, create_snapshot, self_contained_ask,
    AnnotationRequest,
};
use crate::app::{
    command_matches, AgentPhase, AppState, ChatEntry, ComposeTarget, DiffLayout, Effect, InputMode,
    PruneChoice, Screen, VersionChoice,
};
use crate::chat_render::render_markdown;
use crate::config::AppPaths;
use crate::copilot::{
    start_agent, ActivityKind, AgentCommand, AgentEvent, AgentEventEnvelope, AgentRuntime,
    AgentSink, BridgeConfig, LaneEvent, Outbound, OutboundKind,
};
use crate::diff::{DiffLine, LineKind};
use crate::domain::{
    AnchorSide, AnnotationKind, AskMessage, BaseBranchSource, DeliveryState, Placement,
    ReviewContext, SessionRecord,
};
use crate::export::{CommentExport, ExportFormat, ReviewArchive};
use crate::git::Git;
use crate::highlight::{Highlighter, StyledSegment, SyntectHighlighter};
use crate::remote::{PrReference, RemoteResolver};
use crate::storage::{now, Storage};
use crate::work_item::{combine_resolved, resolve_local};

pub(crate) fn run(mut state: AppState, storage: &Storage, paths: &AppPaths) -> Result<()> {
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
    state.pending_comment_ids = storage.pending_comment_delivery_ids(&state.work_item.item.id)?;
    state.pending_context = storage
        .context_for_work_item(&state.work_item.item.id)?
        .is_some_and(|context| context.delivery_state == DeliveryState::Pending);
    let pending_deliveries = state.pending_asks.len()
        + usize::from(!state.pending_comment_ids.is_empty())
        + usize::from(state.pending_context);
    if pending_deliveries > 0 {
        state.previous_screen = Screen::Review;
        state.screen = Screen::Recovery;
        state.status =
            format!("{pending_deliveries} outbound message(s) may not have been delivered");
    }
    let model = storage
        .setting("model")?
        .unwrap_or_else(|| "gpt-5".to_owned());
    state.model = model.clone();
    state.expand_step = storage
        .setting("diff.expand_step")?
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|step| *step > 0)
        .unwrap_or(10);
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
    let bridge = start_agent(BridgeConfig {
        work_item_id: state.work_item.item.id.clone(),
        session_root: state.work_item.session_root.clone(),
        existing_session_id,
        model,
        skill_directories,
    });
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        disable_raw_mode().ok();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            disable_raw_mode().ok();
            execute!(
                io::stdout(),
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
    while !state.should_quit {
        while let Some(event) = bridge.try_recv_laned() {
            handle_agent_envelope(state, storage, event)?;
        }
        terminal.draw(|frame| render(frame, state, highlighter))?;
        if event::poll(std::time::Duration::from_millis(100))? {
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
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => state.scroll_chat_or_diff(-3),
                    MouseEventKind::ScrollDown => state.scroll_chat_or_diff(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    Ok(())
}

fn queue_outbound(
    state: &mut AppState,
    bridge: &dyn AgentSink,
    outbound: Outbound,
) -> Result<String> {
    let outbound_id = outbound.id.clone();
    let side_outbound =
        (state.side_active || state.side_starting) && outbound.kind == OutboundKind::Chat;
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
    state.agent_progress.record(
        AgentPhase::Queued,
        "Waiting in the Copilot SDK queue",
        format!("Outbound {} has not started yet", short_id(&outbound_id)),
        Some(outbound_id.clone()),
    );
    Ok(outbound_id)
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
                state.should_quit = true;
            } else {
                state.status = "Unsubmitted comments/asks exist; use :q! to force quit".into();
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
        }
        Effect::SendChat(text) => {
            if !text.trim().is_empty() {
                let outbound = Outbound::new(OutboundKind::Chat, text.clone());
                let outbound_id = queue_outbound(state, bridge, outbound)?;
                let entry = ChatEntry {
                    role: "you".into(),
                    text: text.clone(),
                    streaming: false,
                    annotation_id: None,
                    outbound_id: Some(outbound_id.clone()),
                    error: None,
                };
                if state.side_starting && !state.side_active {
                    state.pending_side_entries.push(entry);
                } else {
                    state.chat.push(entry);
                }
                follow_chat(state);
                state.status = if state.side_starting {
                    "SIDE message buffered safely while the ephemeral fork starts".into()
                } else {
                    "Chat message queued".into()
                };
            }
        }
        Effect::StartSide(question) => {
            if state.side_active || state.side_starting {
                state.status =
                    "A SIDE conversation is already active or starting · use /main first".into();
            } else {
                let outbound = question.filter(|text| !text.trim().is_empty()).map(|text| {
                    let outbound = Outbound::new(OutboundKind::Chat, text.clone());
                    state.pending_side_entries.push(ChatEntry {
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
                bridge.send(AgentCommand::Abort)?;
                bridge.send(AgentCommand::ExitSide)?;
                state.agent_progress.record(
                    AgentPhase::Stopping,
                    "Cancelling SIDE creation",
                    "Waiting for the SDK fork operation to unwind; MAIN remains unchanged",
                    None,
                );
            } else if state.side_active {
                bridge.send(AgentCommand::ExitSide)?;
                state.agent_progress.record(
                    AgentPhase::Stopping,
                    "Closing SIDE and returning to MAIN",
                    "SIDE is ephemeral; MAIN history remains unchanged",
                    None,
                );
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
                queue_outbound(
                    state,
                    bridge,
                    Outbound::new(
                        OutboundKind::Correction,
                        format!(
                            "Correction to submitted review annotation {}: {}",
                            annotation_id, text
                        ),
                    ),
                )?;
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
            queue_outbound(
                state,
                bridge,
                Outbound::new(
                    OutboundKind::Correction,
                    format!(
                        "Correction to ask annotation {annotation_id}, message {message_id}: {text}"
                    ),
                ),
            )?;
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
                queue_outbound(
                    state,
                    bridge,
                    Outbound::new(
                        OutboundKind::Correction,
                        format!("Correction: review annotation {annotation_id} was deleted."),
                    ),
                )?;
            }
            storage.delete_annotation(&annotation_id)?;
            state
                .annotations
                .retain(|(current, _)| current.id != annotation_id);
            state.ask_threads.remove(&annotation_id);
            state.deleted_annotation = Some((annotation, placements, ask_messages));
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
            for id in &ids {
                prune_work_item(storage, paths, id, export_first)?;
            }
            state.prune_items.retain(|item| !ids.contains(&item.id));
            state.prune_index = state
                .prune_index
                .min(state.prune_items.len().saturating_sub(1));
            state.status = format!(
                "Pruned {} Work Item(s); Copilot transcripts remain in the CLI session store",
                ids.len()
            );
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
            state.chat.push(ChatEntry {
                role: "you (resent)".into(),
                text: message.text.clone(),
                streaming: false,
                annotation_id: Some(annotation.id),
                outbound_id: Some(outbound_id.clone()),
                error: None,
            });
            follow_chat(state);
            finish_recovery_item(state, &message.id);
            state.status = "Pending ask intentionally resent".into();
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
            state.chat.push(ChatEntry {
                role: "comments (resent)".into(),
                text: export.structured_session_message(),
                streaming: false,
                annotation_id: None,
                outbound_id: Some(outbound_id.clone()),
                error: None,
            });
            follow_chat(state);
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
        Effect::SetModel(model) => {
            storage.set_setting("model", &model)?;
            bridge.send(AgentCommand::SetModel(model.clone()))?;
            state.model = model.clone();
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
                state.chat.push(ChatEntry {
                    role: "comments".into(),
                    text: export.structured_session_message(),
                    streaming: false,
                    annotation_id: None,
                    outbound_id: Some(outbound_id),
                    error: None,
                });
                follow_chat(state);
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
        Effect::Preview => match preview_markdown(state, paths) {
            Ok(path) => state.status = format!("Opened preview {}", path.display()),
            Err(error) => state.status = error.to_string(),
        },
        Effect::Yank(text) => {
            copy_to_clipboard(&text)?;
            state.status = format!("Yanked {} bytes", text.len());
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
        Effect::StartSide(question) => {
            for entry in state.pending_side_entries.drain(..) {
                if let Some(id) = entry.outbound_id {
                    state.pending_outbound_ids.remove(&id);
                    state.side_outbound_ids.remove(&id);
                }
            }
            state.side_starting = false;
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
    storage.mark_version_opened(version_id)?;
    if version.kind == crate::domain::VersionKind::Remote {
        for stale in storage.unopened_remote_versions(repo_id, version_id)? {
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

fn prune_work_item(
    storage: &Storage,
    paths: &AppPaths,
    work_item_id: &str,
    export_first: bool,
) -> Result<()> {
    let item = storage
        .work_item_by_id(work_item_id)?
        .context("review history item no longer exists")?;
    if export_first {
        let archive = ReviewArchive::load(storage, &item)?;
        if !archive.annotations.is_empty() {
            let path = paths
                .exports
                .join(format!("{}-pruned.md", item.name.replace(['/', ' '], "-")));
            archive.write_markdown(&path)?;
        }
    }

    let git = Git::default();
    let repos = storage.repos_for_work_item(work_item_id)?;
    let mut remote_cache_roots = Vec::new();
    for repo in &repos {
        for version in storage.versions_for_repo(&repo.id)? {
            match version.kind {
                crate::domain::VersionKind::Remote => {
                    if let Some(worktree) = version.worktree_path {
                        git.remove_worktree(&repo.path, &worktree)?;
                    }
                }
                crate::domain::VersionKind::Snapshot => {
                    git.delete_snapshot_ref(&repo.path, &version.id)?;
                    let materialized = paths
                        .cache
                        .join("history")
                        .join(&repo.id)
                        .join(format!("s{}", version.version_num));
                    git.remove_worktree(&repo.path, &materialized)?;
                }
                crate::domain::VersionKind::WorkingTree => {}
            }
        }
        if repo.remote_pr_url.is_some() {
            if let Some(cache_root) = repo.path.parent() {
                remote_cache_roots.push(cache_root.to_path_buf());
            }
        }
    }
    storage.delete_work_item(work_item_id)?;
    for cache_root in remote_cache_roots {
        if cache_root.starts_with(&paths.prs) && cache_root != paths.prs {
            std::fs::remove_dir_all(cache_root)?;
        }
    }
    let synthetic_root = paths.roots.join(work_item_id);
    if synthetic_root.starts_with(&paths.roots)
        && synthetic_root != paths.roots
        && synthetic_root.exists()
    {
        std::fs::remove_dir_all(synthetic_root)?;
    }
    Ok(())
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let sequence = format!("\u{1b}]52;c;{encoded}\u{7}");
    if io::stdout().write_all(sequence.as_bytes()).is_ok() {
        io::stdout().flush().ok();
        return Ok(());
    }
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("clipboard process has no stdin"))?
        .write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        anyhow::bail!("native clipboard command failed");
    }
    Ok(())
}

fn preview_markdown(state: &AppState, paths: &AppPaths) -> Result<std::path::PathBuf> {
    let markdown = if state.input_mode == InputMode::Compose {
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
    let preview_dir = paths.cache.join("previews");
    std::fs::create_dir_all(&preview_dir)?;
    let path = preview_dir.join(format!("{}.html", state.work_item.item.id));
    let rendered = markdown_to_html(&markdown);
    std::fs::write(
        &path,
        format!(
            "<!doctype html><meta charset=\"utf-8\"><title>rq-tui preview</title>\
             <style>body{{max-width:900px;margin:3rem auto;font:16px/1.55 system-ui}}\
             pre{{padding:1rem;background:#111827;color:#e5e7eb;overflow:auto}}\
             code{{font-family:ui-monospace,monospace}}</style><body>{rendered}</body>"
        ),
    )?;
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener).arg(&path).spawn()?;
    Ok(path)
}

fn markdown_to_html(markdown: &str) -> String {
    let mut output = String::new();
    let mut in_code = false;
    let mut in_list = false;
    for line in markdown.lines() {
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
            continue;
        }
        let escaped = line
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        if in_code {
            output.push_str(&escaped);
            output.push('\n');
        } else if let Some(heading) = escaped.strip_prefix("### ") {
            output.push_str(&format!("<h3>{heading}</h3>"));
        } else if let Some(heading) = escaped.strip_prefix("## ") {
            output.push_str(&format!("<h2>{heading}</h2>"));
        } else if let Some(heading) = escaped.strip_prefix("# ") {
            output.push_str(&format!("<h1>{heading}</h1>"));
        } else if let Some(item) = escaped
            .strip_prefix("- ")
            .or_else(|| escaped.strip_prefix("* "))
        {
            if !in_list {
                output.push_str("<ul>");
                in_list = true;
            }
            output.push_str(&format!("<li>{item}</li>"));
        } else {
            if in_list {
                output.push_str("</ul>");
                in_list = false;
            }
            if escaped.is_empty() {
                output.push_str("<br>");
            } else {
                output.push_str(&format!("<p>{escaped}</p>"));
            }
        }
    }
    if in_code {
        output.push_str("</code></pre>");
    }
    if in_list {
        output.push_str("</ul>");
    }
    output
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
            state.agent_progress.record(
                AgentPhase::Idle,
                format!("SIDE {} ready", short_id(&side_id)),
                "Ephemeral fork active; MAIN history is unchanged",
                None,
            );
            state.status = "SIDE conversation ready · /main returns to MAIN".into();
        }
        LaneEvent::SideExited { side_id, .. } => {
            if !state.side_active && !state.side_starting {
                return Ok(());
            }
            if state
                .side_session_id
                .as_deref()
                .is_some_and(|active| active != side_id)
            {
                return Ok(());
            }
            state.chat = state.main_chat.take().unwrap_or_default();
            state.pending_side_entries.clear();
            state.side_starting = false;
            state.side_active = false;
            state.side_session_id = None;
            state.chat_scroll = 0;
            state.chat_autofollow = true;
            for id in state.side_outbound_ids.drain() {
                state.pending_outbound_ids.remove(&id);
            }
            state.agent_progress.queue_depth = 0;
            state.agent_progress.record(
                AgentPhase::Idle,
                "Back on MAIN",
                "SIDE was discarded; persistent MAIN history was not changed",
                None,
            );
            state.status = "SIDE closed · returned to MAIN".into();
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
            state.agent_progress.queue_depth = 0;
            state.agent_progress.record(
                AgentPhase::Failed,
                "Could not start SIDE",
                message.clone(),
                None,
            );
            state.status = message;
        }
        LaneEvent::Agent(agent_event) => {
            let lane_is_visible = match &lane {
                crate::copilot::AgentLane::Main => !state.side_active,
                crate::copilot::AgentLane::Side { id } => {
                    state.side_active && state.side_session_id.as_deref() == Some(id.as_str())
                }
            };
            if !lane_is_visible {
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
                    ActivityKind::Other => state.agent_progress.phase,
                };
                let detail = match (&activity.tool, &activity.detail) {
                    (Some(tool), Some(detail)) => format!("{tool}: {detail}"),
                    (Some(tool), None) => tool.clone(),
                    (None, Some(detail)) => detail.clone(),
                    (None, None) => "SDK activity event".into(),
                };
                let lane_label = lane.label();
                state.agent_progress.record(
                    phase,
                    format!("{lane_label} · {}", activity.label),
                    detail,
                    agent_event_outbound_id(&agent_event),
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
            state.agent_progress.record(
                AgentPhase::Queued,
                format!("Queued request #{}", position + 1),
                format!(
                    "Waiting for the SDK to start outbound {}",
                    short_id(&outbound_id)
                ),
                Some(outbound_id),
            );
            state.status = format!("Agent message queued at position {}", position + 1);
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
                OutboundKind::Chat | OutboundKind::Correction => None,
            };
            state.chat.push(ChatEntry {
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
            state.pending_outbound_ids.remove(&outbound_id);
            state.side_outbound_ids.remove(&outbound_id);
            let context_completed = state.context_streaming;
            if context_completed {
                state.context_streaming = false;
                state.status = if aborted {
                    "Context generation stopped; the partial draft is editable".into()
                } else {
                    "Context draft ready; edit or accept it".into()
                };
            }
            if let Some(message) = state.chat.iter_mut().find(|message| {
                message.streaming && message.outbound_id.as_deref() == Some(outbound_id.as_str())
            }) {
                message.streaming = false;
                if aborted {
                    message.error = Some("stopped".into());
                }
            }
            if !context_completed {
                state.status = if aborted {
                    "Copilot response stopped".into()
                } else {
                    "Copilot response complete".into()
                };
            }
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
        AgentEvent::TurnFailed {
            outbound_id,
            outbound,
            message,
            response_started,
        } => {
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
        AgentEvent::Activity { label, .. } => {
            state.agent_activity = label.clone();
            state.agent_progress.record(
                AgentPhase::Planning,
                label.clone(),
                "Copilot SDK activity",
                None,
            );
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
        AgentEvent::ModelChanged(model) => {
            state.status = format!("Model changed to {model}");
        }
        AgentEvent::Compacted => state.status = "Session compacted".into(),
        AgentEvent::Error(error) => {
            state.agent_connected = false;
            state.agent_activity = "Disconnected".into();
            state.agent_progress.record(
                AgentPhase::Disconnected,
                "Copilot SDK disconnected",
                error.clone(),
                None,
            );
            for message in state.chat.iter_mut().filter(|message| message.streaming) {
                message.streaming = false;
                message.error = Some("connection lost".into());
            }
            state.status = format!("Copilot unavailable: {error}");
        }
        AgentEvent::Stopped => {
            state.agent_connected = false;
            state.agent_activity = "Disconnected".into();
            state.agent_progress.record(
                AgentPhase::Disconnected,
                "Copilot worker stopped",
                "No SDK event stream is active",
                None,
            );
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
    if frame.area().width < 32 || frame.area().height < 8 {
        let area = frame.area();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(format!(
                "rq-tui needs at least 32×8\ncurrent: {}×{}\nresize the terminal to continue",
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
    }
    if state.input_mode == InputMode::Command {
        render_command_palette(frame, state);
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
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title("Delivery recovery — these asks may not have been delivered")
                .borders(Borders::ALL),
        ),
        frame.area(),
    );
    let area = frame.area();
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
    let base = state
        .work_item
        .repos
        .get(state.repo_index)
        .and_then(|repo| repo.record.base_branch.as_deref())
        .unwrap_or("auto-detect");
    let rows = [
        format!("Model             {}", state.model),
        "Ask tool scope    read/search only (fixed)".into(),
        format!("Base branch       {base} (per repository)"),
        "Keybindings       vim".into(),
        format!(
            "Diff context      6 lines · expand step {}",
            state.expand_step
        ),
        "Markdown preview  system browser".into(),
        "Storage           SQLite (WAL)".into(),
    ];
    let items = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            ListItem::new(format!(
                "{} {row}",
                if index == state.settings_index {
                    "▶"
                } else {
                    " "
                }
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title("Settings · Enter edits model/base/diff step · q back")
                .borders(Borders::ALL),
        ),
        frame.area(),
    );
}

fn render_versions(frame: &mut ratatui::Frame, state: &AppState) {
    let items = state
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
            ListItem::new(format!(
                "{marker} {:<16} {:<5} {}  {} asks · {} comments",
                choice.repo_name, prefix, choice.version.created_at, choice.asks, choice.comments
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title("Version History")
                .borders(Borders::ALL),
        ),
        frame.area(),
    );
}

fn render_prune(frame: &mut ratatui::Frame, state: &AppState) {
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
            let selected = if item.selected { "x" } else { " " };
            ListItem::new(format!(
                "{cursor} [{selected}] {:<28} last reviewed {} · {} versions · {} annotations",
                item.name, item.last_opened_at, item.versions, item.annotations
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title("Prune — reviewed items (oldest first)")
                .borders(Borders::ALL),
        ),
        frame.area(),
    );
}

fn render_review(frame: &mut ratatui::Frame, state: &AppState, highlighter: &mut dyn Highlighter) {
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
    if matches!(
        state.compose_target,
        Some(
            ComposeTarget::Annotation(_)
                | ComposeTarget::FollowUp(_)
                | ComposeTarget::EditAnnotation(_)
                | ComposeTarget::EditAskMessage { .. }
        )
    ) {
        render_contextual_composer(frame, state, body[1]);
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
    let title = format!(
        " {} — Review  │  {} > {}  │  {}/{} files{} ",
        state.work_item.item.name,
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
    let mut items = Vec::new();
    for (repo_index, repo) in state.work_item.repos.iter().enumerate() {
        let collapsed = state.collapsed_repos.contains(&repo.record.id);
        items.push(ListItem::new(Line::styled(
            format!(
                "{} {} ({} files)",
                if collapsed { "▸" } else { "▾" },
                repo.record.name,
                repo.diff.files.len()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if collapsed {
            continue;
        }
        for (file_index, file) in repo.diff.files.iter().enumerate() {
            if state.input_mode == InputMode::Search
                && state.focus == crate::app::Focus::FilePicker
                && !state.search.is_empty()
                && !fuzzy_match(
                    &file.display_path.to_string_lossy().to_lowercase(),
                    &state.search.to_lowercase(),
                )
            {
                continue;
            }
            let marker = if repo_index == state.repo_index && file_index == state.file_index {
                "▶"
            } else {
                " "
            };
            items.push(ListItem::new(format!(
                "{marker}  {}",
                file.display_path.display()
            )));
        }
    }
    frame.render_widget(
        List::new(items).block(Block::default().title("files").borders(Borders::RIGHT)),
        area,
    );
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
    state: &AppState,
    area: Rect,
    highlighter: &mut dyn Highlighter,
) {
    let lines = visible_diff_lines(state, highlighter, area.height as usize);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().title("unified").borders(Borders::NONE))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_split(
    frame: &mut ratatui::Frame,
    state: &AppState,
    area: Rect,
    highlighter: &mut dyn Highlighter,
) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(36),
            Constraint::Percentage(36),
            Constraint::Percentage(28),
        ])
        .split(area);
    let rows = split_rows(state);
    let start = state.scroll.min(rows.len());
    let end = (start + area.height as usize).min(rows.len());
    let old = rows[start..end]
        .iter()
        .map(|(index, old, _)| {
            split_line(
                old.as_ref(),
                state.current_file().map(|file| file.path()),
                old.is_some() && diff_row_selected(state, *index),
                highlighter,
            )
        })
        .collect::<Vec<_>>();
    let new = rows[start..end]
        .iter()
        .map(|(index, _, new)| {
            split_line(
                new.as_ref(),
                state.current_file().map(|file| file.path()),
                new.is_some() && diff_row_selected(state, *index),
                highlighter,
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(old).block(Block::default().title("- old").borders(Borders::RIGHT)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(new).block(Block::default().title("+ new").borders(Borders::RIGHT)),
        columns[1],
    );
    render_annotation_rail(frame, state, columns[2]);
}

fn render_status(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let mode = match state.input_mode {
        InputMode::Normal => "NORMAL",
        InputMode::Visual => "VISUAL",
        InputMode::Command => "COMMAND",
        InputMode::Search => "SEARCH",
        InputMode::Compose => "INSERT",
    };
    let content = match state.input_mode {
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
            "INSERT  Enter/Ctrl-S submit · Shift-Enter newline · Esc cancel".into()
        }
        InputMode::Compose => format!(
            "{mode}  {}  (Enter/Ctrl-S submit · Shift-Enter newline · Esc cancel)",
            state.compose.replace('\n', " ↵ ")
        ),
        InputMode::Visual => {
            let (start, end) = state.selection();
            format!(
                "VISUAL  rows {}-{} · a ask · c comment · y yank · Esc clear  {}",
                start + 1,
                end + 1,
                state.status
            )
        }
        _ => format!(
            "{mode}  j/k move · h/l file · v select · a ask · c comment · Tab chat · : command  {}",
            state.status
        ),
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

fn render_command_palette(frame: &mut ratatui::Frame, state: &AppState) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(96);
    let height = 12.min(area.height.saturating_sub(2)).max(4);
    let palette = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + 1,
        width,
        height,
    );
    let matches = command_matches(&state.command);
    let visible_rows = height.saturating_sub(4) as usize;
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
    let mut lines = vec![Line::styled(
        format!("  :{}█", state.command),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    lines.extend(suggestions);
    lines.push(Line::styled(
        format!(
            "  ↑/↓ select · PgUp/PgDn scroll · Tab complete · Enter run · Esc cancel   {}/{}",
            if matches.is_empty() {
                0
            } else {
                state.command_index.min(matches.len() - 1) + 1
            },
            matches.len()
        ),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Clear, palette);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" COMMAND MODE · Command palette ")
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

fn render_contextual_composer(frame: &mut ratatui::Frame, state: &AppState, body: Rect) {
    if body.width < 20 || body.height < 4 {
        return;
    }
    let title = match &state.compose_target {
        Some(ComposeTarget::Annotation(AnnotationKind::Ask)) => " Ask ",
        Some(ComposeTarget::Annotation(AnnotationKind::Comment)) => " Comment ",
        Some(ComposeTarget::FollowUp(_)) => " Ask follow-up ",
        Some(ComposeTarget::EditAnnotation(_) | ComposeTarget::EditAskMessage { .. }) => " Edit ",
        _ => return,
    };
    let width = body.width.saturating_sub(4).min(76);
    let height = 4.min(body.height);
    let cursor_y = state.cursor.saturating_sub(state.scroll) as u16;
    let preferred_y = body.y.saturating_add(cursor_y).saturating_add(1);
    let max_y = body.bottom().saturating_sub(height);
    let composer = Rect::new(
        body.x + (body.width.saturating_sub(width)) / 2,
        preferred_y.min(max_y),
        width,
        height,
    );
    let range = state
        .current_file()
        .and_then(|file| {
            let selection = state.diff_selection();
            anchor_from_diff(file, selection.start_row, selection.end_row).ok()
        })
        .map(|anchor| {
            format!(
                "{:?} lines {}-{}",
                anchor.side, anchor.line_start, anchor.line_end
            )
            .to_lowercase()
        })
        .unwrap_or_else(|| "selected code".into());
    frame.render_widget(Clear, composer);
    frame.render_widget(
        Paragraph::new(format!("{}\n  {range}", state.compose.replace('\n', " ↵ ")))
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        composer,
    );
}

fn render_chat(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    highlighter: &mut dyn Highlighter,
) {
    let width = frame.area().width.saturating_sub(4).max(1) as usize;
    let composer_height = chat_composer_height(state, width, frame.area().height);
    let progress_height = if frame.area().height >= 14 { 4 } else { 3 };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(progress_height),
            Constraint::Length(composer_height),
        ])
        .split(frame.area());

    let (mut lines, ranges) = chat_lines(state, width, highlighter);
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
        if let Some((start, end)) = ranges.get(state.chat_cursor).copied() {
            if start < state.chat_scroll {
                state.chat_scroll = start;
            } else if end >= state.chat_scroll + viewport_rows {
                state.chat_scroll = end.saturating_add(1).saturating_sub(viewport_rows);
            }
        }
    }
    let lane = visible_lane(state);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .title(format!(
                        " {} — Chat · {lane} · rows {}-{}/{} ",
                        state.work_item.item.name,
                        state
                            .chat_scroll
                            .saturating_add(1)
                            .min(state.chat_total_rows.max(1)),
                        (state.chat_scroll + viewport_rows).min(state.chat_total_rows),
                        state.chat_total_rows,
                    ))
                    .borders(Borders::ALL),
            )
            .scroll((state.chat_scroll.min(u16::MAX as usize) as u16, 0)),
        vertical[0],
    );
    render_agent_progress(frame, state, vertical[1]);
    render_chat_composer(frame, state, vertical[2]);
}

fn chat_lines(
    state: &AppState,
    width: usize,
    highlighter: &mut dyn Highlighter,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let mut lines = Vec::new();
    let mut ranges = Vec::with_capacity(state.chat.len());
    for (index, message) in state.chat.iter().enumerate() {
        let start = lines.len();
        let selected = index == state.chat_cursor
            || state.chat_visual_anchor.is_some_and(|anchor| {
                anchor.min(state.chat_cursor) <= index && index <= anchor.max(state.chat_cursor)
            });
        let stopped = message.error.as_deref() == Some("stopped");
        let marker = if message.streaming {
            " ◐ streaming"
        } else if stopped {
            " ■ stopped"
        } else if message.error.is_some() {
            " ⚠ failed"
        } else if message.role != "copilot"
            && message.outbound_id.as_deref()
                == state.agent_progress.active_outbound_id.as_deref()
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
                    if selected { "▶ " } else { "  " },
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
            .style(if selected {
                Style::default().bg(Color::Rgb(40, 50, 65))
            } else {
                Style::default()
            }),
        );
        let body_width = width.saturating_sub(2).max(1);
        let body = render_markdown(&message.text, body_width, highlighter);
        lines.extend(body.into_iter().map(|line| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(line.spans);
            Line::from(spans).style(if selected {
                Style::default().bg(Color::Rgb(40, 50, 65))
            } else {
                Style::default()
            })
        }));
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
        lines.push(Line::from(""));
        ranges.push((start, lines.len().saturating_sub(1)));
    }
    (lines, ranges)
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

    let (title, border_color) = match state.input_mode {
        InputMode::Compose => (
            " CHAT INPUT · INSERT · Enter send · Shift-Enter newline · Esc normal · Ctrl-C discard "
                .to_owned(),
            Color::Green,
        ),
        InputMode::Command => (
            " COMMAND MODE ACTIVE ↑ USE PALETTE · Esc cancel ".to_owned(),
            Color::Cyan,
        ),
        InputMode::Search => (
            " SEARCH MODE · Enter next · Esc cancel ".to_owned(),
            Color::Yellow,
        ),
        InputMode::Visual => {
            let anchor = state.chat_visual_anchor.unwrap_or(state.chat_cursor);
            (
                format!(
                    " VISUAL  messages {}-{} · y copy · Esc normal ",
                    anchor.min(state.chat_cursor) + 1,
                    anchor.max(state.chat_cursor) + 1
                ),
                Color::Magenta,
            )
        }
        InputMode::Normal => (
            " CHAT INPUT · NORMAL · i edit · j/k scroll · G latest · Ctrl-C stop ".to_owned(),
            Color::DarkGray,
        ),
    };
    let display = if state.input_mode == InputMode::Search {
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
    let mut lines = vec![String::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    for (byte, character) in text.char_indices() {
        if byte == cursor {
            cursor_row = row;
            cursor_col = col;
        }
        if character == '\n' {
            lines.push(String::new());
            row += 1;
            col = 0;
            continue;
        }
        let rendered = if character == '\t' { ' ' } else { character };
        let char_width = terminal_char_width(rendered);
        if col > 0 && col + char_width > width {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        lines[row].push(rendered);
        col += char_width;
    }
    if cursor == text.len() {
        cursor_row = row;
        cursor_col = col;
    }
    (lines, cursor_row, cursor_col)
}

fn terminal_char_width(character: char) -> usize {
    if character.is_control() {
        0
    } else if matches!(
        character as u32,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1faff
    ) {
        2
    } else {
        1
    }
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
    let mut lines = vec![Line::from(vec![Span::styled(
        format!(
            " COPILOT {lane} {state_marker} {} {} · last SDK event {} · event #{} · queue {} ",
            progress.phase.label(),
            format_duration(progress.elapsed()),
            format_duration(age),
            progress.event_count,
            progress.queue_depth,
        ),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )])];
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
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::TOP | Borders::BOTTOM)),
        area,
    );
}

fn render_agent_status(frame: &mut ratatui::Frame, state: &AppState) {
    let progress = &state.agent_progress;
    let lane = visible_lane(state);
    let mut lines = vec![
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
            "Recent SDK activity (newest last)",
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    let now = std::time::Instant::now();
    lines.extend(progress.timeline.iter().skip(state.scroll).map(|entry| {
        Line::raw(format!(
            "  {:>7} ago  {}",
            format_duration(now.saturating_duration_since(entry.at)),
            entry.label
        ))
    }));
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "q/Esc return · j/k scroll · s stop current response",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Agent status · explicit stuck diagnostics ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        frame.area(),
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
        | AgentEvent::ResponseStarted { outbound_id, .. }
        | AgentEvent::ResponseDelta { outbound_id, .. }
        | AgentEvent::ResponseSnapshot { outbound_id, .. }
        | AgentEvent::ResponseComplete { outbound_id, .. }
        | AgentEvent::TurnFailed { outbound_id, .. } => Some(outbound_id.clone()),
        AgentEvent::Activity { outbound_id, .. } => outbound_id.clone(),
        _ => None,
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn render_annotation_rail(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let Some(repo) = state.work_item.repos.get(state.repo_index) else {
        return;
    };
    let Some(file) = state.current_file() else {
        return;
    };
    let rows = state
        .annotations
        .iter()
        .filter(|(annotation, _)| {
            annotation.repo_id == repo.record.id && annotation.file_path == file.display_path
        })
        .map(|(annotation, placement)| {
            let marker = if placement.outdated {
                "!"
            } else if placement.ambiguous {
                "≈"
            } else {
                "▸"
            };
            let kind = match annotation.kind {
                AnnotationKind::Ask => "a",
                AnnotationKind::Comment => "c",
            };
            let text = if annotation.kind == AnnotationKind::Ask {
                state
                    .ask_threads
                    .get(&annotation.id)
                    .and_then(|thread| thread.last())
                    .map(|message| format!("{}: {}", message.role, message.text.replace('\n', " ")))
                    .unwrap_or_else(|| "(ask queued)".into())
            } else {
                annotation
                    .text
                    .as_deref()
                    .unwrap_or_default()
                    .replace('\n', " ")
            };
            ListItem::new(format!(
                "{marker} [{kind}] ln {}-{} {text}",
                placement.line_start, placement.line_end
            ))
        })
        .collect::<Vec<_>>();
    let rows = if rows.is_empty() {
        vec![ListItem::new("No annotations")]
    } else {
        rows
    };
    frame.render_widget(
        List::new(rows).block(
            Block::default()
                .title("ask / comments (this file)")
                .borders(Borders::NONE),
        ),
        area,
    );
}

fn visible_diff_lines(
    state: &AppState,
    highlighter: &mut dyn Highlighter,
    height: usize,
) -> Vec<Line<'static>> {
    let Some(file) = state.current_file() else {
        return vec![Line::from("No changed files")];
    };
    let current_repo_id = state
        .work_item
        .repos
        .get(state.repo_index)
        .map(|repo| repo.record.id.as_str())
        .unwrap_or_default();
    file.visible_lines()
        .enumerate()
        .skip(state.scroll)
        .take(height)
        .flat_map(|(index, line)| {
            let selected = diff_row_selected(state, index);
            let mut rendered = vec![unified_line(
                file.path(),
                line,
                index,
                selected,
                highlighter,
            )];
            for (annotation, placement) in &state.annotations {
                if annotation.repo_id == current_repo_id
                    && annotation.file_path == file.display_path
                    && placement_line(line, placement.side)
                        .is_some_and(|display_line| placement.line_start == display_line as i64)
                {
                    let marker = if placement.outdated {
                        "!"
                    } else if placement.ambiguous {
                        "≈"
                    } else {
                        "↳"
                    };
                    let kind = match annotation.kind {
                        AnnotationKind::Ask => "ask",
                        AnnotationKind::Comment => "comment",
                    };
                    let collapsed = state.collapsed_annotations.contains(&annotation.id);
                    rendered.push(Line::styled(
                        format!(
                            "        {marker} [{kind}] {}",
                            if collapsed { "[collapsed]" } else { "" }
                        ),
                        Style::default().fg(Color::Cyan),
                    ));
                    if !collapsed {
                        match annotation.kind {
                            AnnotationKind::Comment => rendered.push(Line::styled(
                                format!(
                                    "          you: {}",
                                    annotation.text.as_deref().unwrap_or_default()
                                ),
                                Style::default().fg(Color::Green),
                            )),
                            AnnotationKind::Ask => {
                                if let Some(thread) = state.ask_threads.get(&annotation.id) {
                                    rendered.extend(thread.iter().map(|message| {
                                        let speaker = if message.role == "assistant" {
                                            "copilot"
                                        } else {
                                            "you"
                                        };
                                        let waiting =
                                            if message.delivery_state == DeliveryState::Pending {
                                                " ⠋"
                                            } else {
                                                ""
                                            };
                                        Line::styled(
                                            format!(
                                                "          {speaker}{waiting}: {}",
                                                message.text.replace('\n', " ")
                                            ),
                                            Style::default().fg(if message.role == "assistant" {
                                                Color::Cyan
                                            } else {
                                                Color::Green
                                            }),
                                        )
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            rendered
        })
        .collect()
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

fn split_rows(state: &AppState) -> Vec<(usize, Option<DiffLine>, Option<DiffLine>)> {
    let Some(file) = state.current_file() else {
        return Vec::new();
    };
    file.visible_lines()
        .cloned()
        .enumerate()
        .map(|(index, line)| match line.kind {
            LineKind::Deletion => (index, Some(line), None),
            LineKind::Addition => (index, None, Some(line)),
            LineKind::Context | LineKind::Meta => (index, Some(line.clone()), Some(line)),
        })
        .collect()
}

fn diff_row_selected(state: &AppState, index: usize) -> bool {
    if state.input_mode == InputMode::Visual || state.compose_target.is_some() {
        let (start, end) = state.selection();
        start <= index && index <= end
    } else {
        index == state.cursor
    }
}

fn placement_line(line: &DiffLine, side: AnchorSide) -> Option<usize> {
    match side {
        AnchorSide::Old => line.old_line,
        AnchorSide::New => line.new_line,
    }
}

fn split_line(
    line: Option<&DiffLine>,
    path: Option<&std::path::Path>,
    selected: bool,
    highlighter: &mut dyn Highlighter,
) -> Line<'static> {
    let Some(line) = line else {
        return Line::from("");
    };
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
    if selected {
        rendered = rendered.style(Style::default().bg(Color::Rgb(40, 50, 65)));
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
    use std::path::PathBuf;
    use std::sync::Mutex;

    use anyhow::Result;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::{
        handle_agent_event, handle_effect, markdown_to_html, parse_review_context, render,
    };
    use crate::app::{tests_support::state_for_ui, Effect};
    use crate::config::AppPaths;
    use crate::copilot::{AgentCommand, AgentEvent, AgentSink, HistoryEntry, OutboundKind};
    use crate::highlight::PlainHighlighter;
    use crate::storage::Storage;

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

    fn paths() -> AppPaths {
        AppPaths {
            data: PathBuf::from("/tmp/rq-tui-test/data"),
            cache: PathBuf::from("/tmp/rq-tui-test/cache"),
            database: PathBuf::from("/tmp/rq-tui-test/data/db"),
            roots: PathBuf::from("/tmp/rq-tui-test/data/roots"),
            prs: PathBuf::from("/tmp/rq-tui-test/cache/prs"),
            exports: PathBuf::from("/tmp/rq-tui-test/data/exports"),
            skills: PathBuf::from("/tmp/rq-tui-test/data/skills"),
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
        assert!(content.contains("+ new"));
        assert!(content.contains("a ask"));
    }

    #[test]
    fn side_effects_can_use_a_fake_agent_without_starting_copilot() {
        let storage = Storage::in_memory().unwrap();
        let agent = FakeAgent::default();
        let mut state = state_for_ui();
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
    fn markdown_preview_renders_headings_lists_and_code_safely() {
        let html = markdown_to_html("# Review\n\n- item\n\n```\n<a>\n```");
        assert!(html.contains("<h1>Review</h1>"));
        assert!(html.contains("<li>item</li>"));
        assert!(html.contains("&lt;a&gt;"));
    }
}

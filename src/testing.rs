//! Headless helpers for exercising the complete TUI reducer/effect/render loop.

use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{Context, Result};
use crossterm::event::KeyEvent;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;
use tempfile::TempDir;

use crate::app::{AppState, ChatEntry, Effect, InputMode, Screen};
use crate::config::AppPaths;
use crate::copilot::{
    deterministic_outbound_ids, ActivityKind, AgentActivity, AgentCommand, AgentEvent,
    AgentEventEnvelope, AgentLane, AgentSink, ContextTierOption, HistoryEntry, LaneEvent,
    ModelOption, Outbound,
};
use crate::diff::{parse_unified, DiffSet};
use crate::domain::{BaseBranchSource, DeliveryState, Repo, Version, VersionKind, WorkItem};
use crate::highlight::PlainHighlighter;
use crate::storage::Storage;
use crate::ui::{
    handle_agent_envelope, handle_agent_event, handle_effect, handle_effect_failure, render,
};
use crate::work_item::{ResolvedWorkItem, ReviewRepo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedAnnotation {
    pub id: String,
    pub kind: String,
    pub file: String,
    pub side: String,
    pub line_start: i64,
    pub line_end: i64,
    pub text: Option<String>,
    pub submitted: bool,
    pub delivery_state: String,
}

#[derive(Default)]
struct FakeAgent {
    commands: Mutex<Vec<AgentCommand>>,
    pending: Mutex<VecDeque<Outbound>>,
    fail_sends: Mutex<bool>,
}

impl AgentSink for FakeAgent {
    fn send(&self, command: AgentCommand) -> Result<()> {
        if *self.fail_sends.lock().expect("agent lock") {
            anyhow::bail!("fake agent command channel is closed");
        }
        match &command {
            AgentCommand::Send(outbound) | AgentCommand::SendSide(outbound) => {
                self.pending
                    .lock()
                    .expect("agent lock")
                    .push_back(outbound.clone());
            }
            AgentCommand::StartSide {
                outbound: Some(outbound),
            } => {
                self.pending
                    .lock()
                    .expect("agent lock")
                    .push_back(outbound.clone());
            }
            AgentCommand::ReplaceQueued {
                outbound_id,
                replacement,
            } => {
                let mut pending = self.pending.lock().expect("agent lock");
                if let Some(position) = pending
                    .iter()
                    .position(|outbound| outbound.id == *outbound_id)
                {
                    pending[position] = replacement.clone();
                }
            }
            _ => {}
        }
        self.commands.lock().expect("agent lock").push(command);
        Ok(())
    }
}

/// A complete in-memory terminal backed by a temporary on-disk SQLite database
/// and a deterministic fake agent.
pub struct TuiHarness {
    state: AppState,
    storage: Storage,
    paths: AppPaths,
    agent: FakeAgent,
    effects: Vec<String>,
    last_yank: Option<String>,
    active_stream: Option<(Outbound, AgentLane)>,
    width: u16,
    height: u16,
    _temp: TempDir,
}

impl TuiHarness {
    pub(crate) fn new(state: AppState, width: u16, height: u16) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let paths = fixture_paths(temp.path());
        let storage = Storage::open(&paths.database)?;
        seed_storage(&storage, &state)?;
        let mut state = state;
        let display_now = state.agent_progress.last_event_at;
        state.agent_progress.freeze_display_clock(display_now);
        Ok(Self {
            state,
            storage,
            paths,
            agent: FakeAgent::default(),
            effects: Vec::new(),
            last_yank: None,
            active_stream: None,
            width,
            height,
            _temp: temp,
        })
    }

    pub fn from_unified_diff(
        name: impl Into<String>,
        unified_diff: &str,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        let name = name.into();
        let diff = parse_unified(unified_diff)?;
        Self::new(fixture_state(name, diff), width, height)
    }

    /// Sends a key through the reducer and executes every resulting effect
    /// against the temporary database and fake agent.
    pub fn key(&mut self, key: KeyEvent) -> Result<Vec<String>> {
        let effects = self.state.handle_key(key);
        let descriptions = effects
            .iter()
            .map(|effect| format!("{effect:?}"))
            .collect::<Vec<_>>();
        for effect in effects {
            self.effects.push(format!("{effect:?}"));
            if let Effect::Yank(text) = &effect {
                self.last_yank = Some(text.clone());
                self.state.status = format!("Yanked {} bytes", text.len());
                continue;
            }
            let result = handle_effect(
                &mut self.state,
                &self.storage,
                &self.paths,
                &self.agent,
                effect.clone(),
            );
            if let Err(error) = result {
                handle_effect_failure(&mut self.state, &effect, &error);
            } else if let Effect::SteerChat(_) = effect {
                let command = self
                    .agent
                    .commands
                    .lock()
                    .expect("agent lock")
                    .last()
                    .cloned();
                let Some(AgentCommand::Steer(steering)) = command else {
                    anyhow::bail!("fake agent did not receive steering");
                };
                let active_outbound_id = self
                    .state
                    .agent_progress
                    .active_outbound_id
                    .clone()
                    .context("steering has no active fake-agent turn")?;
                handle_agent_event(
                    &mut self.state,
                    &self.storage,
                    AgentEvent::SteeringAccepted {
                        steering_id: steering.id,
                        active_outbound_id,
                    },
                )?;
            } else if let Effect::CancelQueued(outbound_id) = effect {
                self.agent
                    .pending
                    .lock()
                    .expect("agent lock")
                    .retain(|outbound| outbound.id != outbound_id);
                handle_agent_event(
                    &mut self.state,
                    &self.storage,
                    AgentEvent::QueueCancelled { outbound_id },
                )?;
            } else if let Effect::ReplaceQueued { outbound_id, .. } = effect {
                let command = self
                    .agent
                    .commands
                    .lock()
                    .expect("agent lock")
                    .last()
                    .cloned();
                let Some(AgentCommand::ReplaceQueued { replacement, .. }) = command else {
                    anyhow::bail!("fake agent did not receive queue replacement");
                };
                let position = self
                    .agent
                    .pending
                    .lock()
                    .expect("agent lock")
                    .iter()
                    .position(|outbound| outbound.id == replacement.id);
                let event = if let Some(position) = position {
                    AgentEvent::QueueReplaced {
                        outbound_id,
                        replacement_id: replacement.id,
                        position,
                    }
                } else {
                    AgentEvent::QueueReplaceRejected {
                        outbound_id,
                        replacement_id: replacement.id,
                        original_active: false,
                        reason: "prompt already left the queue".into(),
                    }
                };
                handle_agent_event(&mut self.state, &self.storage, event)?;
            }
        }
        Ok(descriptions)
    }

    pub fn paste(&mut self, text: &str) -> Result<()> {
        anyhow::ensure!(
            self.state.handle_paste(text).is_empty(),
            "paste unexpectedly emitted an effect"
        );
        Ok(())
    }

    pub fn mouse_scroll(&mut self, up: bool) -> Result<()> {
        let kind = if up {
            crossterm::event::MouseEventKind::ScrollUp
        } else {
            crossterm::event::MouseEventKind::ScrollDown
        };
        anyhow::ensure!(
            crate::ui::mouse_scroll_effects(&mut self.state, kind).is_empty(),
            "mouse scrolling unexpectedly emitted an effect"
        );
        Ok(())
    }

    pub fn append_history(&mut self, role: impl Into<String>, text: impl Into<String>) {
        self.state.chat.push(ChatEntry {
            id: uuid::Uuid::new_v4().to_string(),
            role: role.into(),
            text: text.into(),
            streaming: false,
            annotation_id: None,
            outbound_id: None,
            error: None,
        });
        self.state.chat_cursor = self.state.chat.len().saturating_sub(1);
        self.state.chat_autofollow = true;
    }

    /// Starts, but deliberately does not complete, the oldest fake-agent turn.
    pub fn start_next_response(&mut self, first_delta: &str) -> Result<()> {
        let lane = if self.state.side_active {
            AgentLane::Side {
                id: self
                    .state
                    .side_session_id
                    .clone()
                    .context("SIDE is active without a session id")?,
            }
        } else {
            AgentLane::Main
        };
        self.start_next_response_on_lane(lane, first_delta)
    }

    pub(crate) fn start_next_response_on_lane(
        &mut self,
        lane: AgentLane,
        first_delta: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            self.active_stream.is_none(),
            "a fake-agent response is already streaming"
        );
        let wants_side = matches!(lane, AgentLane::Side { .. });
        let outbound = {
            let mut pending = self.agent.pending.lock().expect("agent lock");
            let position = pending
                .iter()
                .position(|outbound| {
                    self.state.side_outbound_ids.contains(&outbound.id) == wants_side
                })
                .context("no queued fake-agent outbound on the requested lane")?;
            pending
                .remove(position)
                .expect("located fake-agent outbound remains queued")
        };
        handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane: lane.clone(),
                event: LaneEvent::Agent(AgentEvent::ResponseStarted {
                    outbound_id: outbound.id.clone(),
                    outbound: outbound.kind.clone(),
                    first_delta: first_delta.to_owned(),
                }),
                activity: None,
            },
        )?;
        self.active_stream = Some((outbound, lane));
        Ok(())
    }

    pub fn push_response_delta(&mut self, delta: &str) -> Result<()> {
        let (outbound, lane) = self
            .active_stream
            .as_ref()
            .context("no fake-agent response is streaming")?;
        handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane: lane.clone(),
                event: LaneEvent::Agent(AgentEvent::ResponseDelta {
                    outbound_id: outbound.id.clone(),
                    delta: delta.to_owned(),
                }),
                activity: None,
            },
        )
    }

    pub fn complete_response(&mut self, aborted: bool) -> Result<()> {
        let (outbound, lane) = self
            .active_stream
            .take()
            .context("no fake-agent response is streaming")?;
        handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane,
                event: LaneEvent::Agent(AgentEvent::ResponseComplete {
                    outbound_id: outbound.id,
                    aborted,
                }),
                activity: None,
            },
        )
    }

    /// Injects a successful streamed response for the oldest queued outbound.
    pub fn stream_next_response(&mut self, chunks: &[&str]) -> Result<()> {
        let mut chunks = chunks.iter();
        self.start_next_response(chunks.next().copied().unwrap_or_default())?;
        for chunk in chunks {
            self.push_response_delta(chunk)?;
        }
        self.complete_response(false)
    }

    /// Injects a failed delivery for the oldest queued outbound.
    pub fn fail_next_response(&mut self, message: impl Into<String>) -> Result<()> {
        let outbound = self
            .agent
            .pending
            .lock()
            .expect("agent lock")
            .pop_front()
            .context("no queued fake-agent outbound")?;
        handle_agent_event(
            &mut self.state,
            &self.storage,
            AgentEvent::TurnFailed {
                outbound_id: outbound.id,
                outbound: outbound.kind,
                message: message.into(),
                response_started: false,
            },
        )
    }

    /// Simulates a fresh process reading the same persisted review database.
    pub fn restart(&mut self) -> Result<()> {
        self.agent.pending.lock().expect("agent lock").clear();
        self.active_stream = None;
        let work_item = self.state.work_item.clone();
        let mut state = AppState::new(work_item);
        for repo in &state.work_item.repos {
            state
                .annotations
                .extend(self.storage.annotations_for_version(&repo.version.id)?);
        }
        for (annotation, _) in &state.annotations {
            if annotation.kind.as_str() == "ask" {
                state.ask_threads.insert(
                    annotation.id.clone(),
                    self.storage.ask_messages_for_annotation(&annotation.id)?,
                );
            }
        }
        state.pending_asks = self
            .storage
            .pending_ask_messages(&state.work_item.item.id)?;
        state.pending_chats = self.storage.pending_chats(&state.work_item.item.id)?;
        state.pending_comment_ids = self
            .storage
            .pending_comment_delivery_ids(&state.work_item.item.id)?;
        state.pending_context = self
            .storage
            .context_for_work_item(&state.work_item.item.id)?
            .is_some_and(|context| context.delivery_state == DeliveryState::Pending);
        if !state.pending_asks.is_empty()
            || !state.pending_chats.is_empty()
            || !state.pending_comment_ids.is_empty()
            || state.pending_context
        {
            state.screen = Screen::Recovery;
            state.status = "Outbound delivery is uncertain; resend or discard explicitly".into();
        }
        let display_now = state.agent_progress.last_event_at;
        state.agent_progress.freeze_display_clock(display_now);
        self.state = state;
        Ok(())
    }

    /// Makes SQLite reject writes so persistence-failure recovery can be tested.
    pub fn set_storage_read_only(&self, enabled: bool) -> Result<()> {
        self.storage.set_query_only_for_testing(enabled)
    }

    pub fn set_agent_send_failure(&self, enabled: bool) {
        *self.agent.fail_sends.lock().expect("agent lock") = enabled;
    }

    pub(crate) fn inject_agent_event(&mut self, event: AgentEvent) -> Result<()> {
        handle_agent_event(&mut self.state, &self.storage, event)
    }

    #[cfg(test)]
    pub(crate) fn inject_laned_agent_event(
        &mut self,
        lane: AgentLane,
        event: AgentEvent,
    ) -> Result<()> {
        crate::ui::handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane,
                event: LaneEvent::Agent(event),
                activity: None,
            },
        )
    }

    pub(crate) fn inject_activity(
        &mut self,
        kind: ActivityKind,
        label: impl Into<String>,
        tool: Option<String>,
        detail: Option<String>,
    ) -> Result<()> {
        let label = label.into();
        let lane = self
            .state
            .side_session_id
            .clone()
            .map(|id| AgentLane::Side { id })
            .unwrap_or(AgentLane::Main);
        let outbound_id = self.state.agent_progress.active_outbound_id.clone();
        crate::ui::handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane,
                event: LaneEvent::Agent(AgentEvent::Activity {
                    outbound_id,
                    label: label.clone(),
                }),
                activity: Some(AgentActivity {
                    kind,
                    label,
                    tool,
                    detail,
                }),
            },
        )
    }

    pub fn inject_side_started(
        &mut self,
        parent_id: impl Into<String>,
        side_id: impl Into<String>,
    ) -> Result<()> {
        let side_id = side_id.into();
        crate::ui::handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane: AgentLane::Side {
                    id: side_id.clone(),
                },
                event: LaneEvent::SideStarted {
                    parent_id: parent_id.into(),
                    side_id,
                },
                activity: None,
            },
        )
    }

    pub fn inject_side_exited(
        &mut self,
        parent_id: impl Into<String>,
        side_id: impl Into<String>,
    ) -> Result<()> {
        let side_id = side_id.into();
        crate::ui::handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane: AgentLane::Side {
                    id: side_id.clone(),
                },
                event: LaneEvent::SideExited {
                    parent_id: parent_id.into(),
                    side_id,
                    cleanup_warning: None,
                },
                activity: None,
            },
        )
    }

    pub fn inject_side_failed(&mut self, message: impl Into<String>) -> Result<()> {
        crate::ui::handle_agent_envelope(
            &mut self.state,
            &self.storage,
            AgentEventEnvelope {
                lane: AgentLane::Main,
                event: LaneEvent::SideFailed {
                    message: message.into(),
                },
                activity: None,
            },
        )
    }

    pub fn backdate_agent_progress(&mut self, age: std::time::Duration) {
        let now = std::time::Instant::now();
        self.state.agent_progress.freeze_display_clock(now);
        self.state.agent_progress.last_event_at = now.checked_sub(age).unwrap_or(now);
        self.state.agent_progress.turn_started_at = Some(now.checked_sub(age).unwrap_or(now));
    }

    pub fn chat_scroll(&self) -> usize {
        self.state.chat_scroll
    }

    pub fn compose_text(&self) -> &str {
        &self.state.compose
    }

    pub fn render(&mut self) -> Result<String> {
        let terminal = self.draw()?;
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for row in 0..self.height {
            let line = (0..self.width)
                .filter_map(|column| {
                    let cell = &buffer[(column, row)];
                    (!cell.skip).then(|| cell.symbol())
                })
                .collect::<String>();
            output.push_str(line.trim_end());
            output.push('\n');
        }
        Ok(output)
    }

    /// Counts cells carrying the restrained Visual-selection background.
    pub fn selected_cell_count(&mut self) -> Result<usize> {
        let terminal = self.draw()?;
        Ok(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .filter(|cell| cell.bg == Color::Rgb(40, 50, 65))
            .count())
    }

    pub fn selected_cell_count_in_columns(&mut self, start: u16, end: u16) -> Result<usize> {
        let terminal = self.draw()?;
        let buffer = terminal.backend().buffer();
        Ok((0..self.height)
            .flat_map(|row| (start..end.min(self.width)).map(move |column| (column, row)))
            .filter(|(column, row)| buffer[(*column, *row)].bg == Color::Rgb(40, 50, 65))
            .count())
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    pub fn status(&self) -> &str {
        &self.state.status
    }

    pub fn mode(&self) -> &str {
        match self.state.input_mode {
            InputMode::Normal => "NORMAL",
            InputMode::Visual => "VISUAL",
            InputMode::Command => "COMMAND",
            InputMode::Search => "SEARCH",
            InputMode::Compose => "INSERT",
        }
    }

    pub fn captured_effects(&self) -> &[String] {
        &self.effects
    }

    pub fn last_yank(&self) -> Option<&str> {
        self.last_yank.as_deref()
    }

    pub fn agent_commands(&self) -> Vec<String> {
        self.agent
            .commands
            .lock()
            .expect("agent lock")
            .iter()
            .map(|command| format!("{command:?}"))
            .collect()
    }

    pub fn persisted_annotations(&self) -> Result<Vec<PersistedAnnotation>> {
        let version = &self.state.work_item.repos[self.state.repo_index].version;
        self.storage
            .annotations_for_version(&version.id)?
            .into_iter()
            .map(|(annotation, placement)| {
                Ok(PersistedAnnotation {
                    id: annotation.id,
                    kind: annotation.kind.as_str().to_owned(),
                    file: annotation.file_path.display().to_string(),
                    side: placement.side.as_str().to_owned(),
                    line_start: placement.line_start,
                    line_end: placement.line_end,
                    text: annotation.text,
                    submitted: annotation.submitted,
                    delivery_state: annotation.delivery_state.as_str().to_owned(),
                })
            })
            .collect()
    }

    pub fn pending_outbound_count(&self) -> usize {
        self.state.pending_outbound_ids.len()
    }

    fn draw(&mut self) -> Result<Terminal<TestBackend>> {
        self.state.tick(std::time::Instant::now());
        let backend = TestBackend::new(self.width, self.height);
        let mut terminal = Terminal::new(backend)?;
        let mut highlighter = PlainHighlighter;
        terminal.draw(|frame| render(frame, &mut self.state, &mut highlighter))?;
        Ok(terminal)
    }
}

fn fixture_paths(root: &std::path::Path) -> AppPaths {
    AppPaths {
        data: root.join("data"),
        cache: root.join("cache"),
        database: root.join("data/review.db"),
        roots: root.join("data/roots"),
        prs: root.join("cache/prs"),
        exports: root.join("data/exports"),
        skills: root.join("data/skills"),
        plugins: root.join("data/plugins"),
    }
}

fn seed_storage(storage: &Storage, state: &AppState) -> Result<()> {
    storage.upsert_work_item(&state.work_item.item)?;
    for review_repo in &state.work_item.repos {
        storage.upsert_repo(&review_repo.record)?;
        storage.upsert_version(&review_repo.version)?;
    }
    Ok(())
}

fn fixture_state(name: String, diff: DiffSet) -> AppState {
    AppState::new(ResolvedWorkItem {
        item: WorkItem {
            id: "test-work-item".into(),
            name,
            workspace_root: "/test".into(),
            created_at: String::new(),
            updated_at: String::new(),
            last_opened_at: None,
        },
        repos: vec![ReviewRepo {
            record: Repo {
                id: "test-repo".into(),
                work_item_id: "test-work-item".into(),
                name: "repo".into(),
                path: "/test/repo".into(),
                remote_pr_url: Some("https://example.invalid/repo/pull/1".into()),
                pr_meta_json: None,
                base_branch: Some("main".into()),
                base_branch_source: BaseBranchSource::Auto,
                last_activity_at: None,
            },
            version: Version {
                id: "test-version".into(),
                repo_id: "test-repo".into(),
                version_num: 1,
                kind: VersionKind::Remote,
                created_at: String::new(),
                head_sha: "test".into(),
                worktree_path: Some("/test/repo".into()),
                last_opened_at: None,
            },
            diff,
        }],
        session_root: "/test".into(),
    })
}

/// Render a deterministic, production-data-free UI state through the same
/// reducer/effect/renderer path as the interactive binary.
pub fn render_ui_scenario(name: &str, width: u16, height: u16) -> Result<String> {
    let _deterministic_ids = deterministic_outbound_ids();
    const DIFF: &str = concat!(
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "--- a/src/lib.rs\n",
        "+++ b/src/lib.rs\n",
        "@@ -1,3 +1,4 @@\n",
        " fn review() {\n",
        "+    let visible = true;\n",
        "     finish();\n",
        " }\n",
    );
    if name == "all" {
        let mut gallery = String::new();
        for scenario in [
            "review", "ask", "command", "composer", "quiet", "queue", "side", "model", "settings",
            "markdown", "tiny",
        ] {
            gallery.push_str(&format!("\n===== {scenario} =====\n"));
            gallery.push_str(&render_ui_scenario(scenario, width, height)?);
        }
        return Ok(gallery);
    }

    let mut harness = TuiHarness::from_unified_diff(name, DIFF, width, height)?;
    let press = |harness: &mut TuiHarness, code| {
        harness.key(KeyEvent::new(code, crossterm::event::KeyModifiers::NONE))
    };
    let type_into = |harness: &mut TuiHarness, text: &str| -> Result<()> {
        for character in text.chars() {
            press(harness, crossterm::event::KeyCode::Char(character))?;
        }
        Ok(())
    };

    match name {
        "review" => {}
        "ask" => {
            press(&mut harness, crossterm::event::KeyCode::Char('a'))?;
            type_into(
                &mut harness,
                "How does this work here? How can we make it better? What do we need to do to make it better? Why did the old editor overflow so easily? This Ask editor now expands based on wrapped rows, keeps the selected code visible around it, and scrolls internally when the prompt becomes taller than its safe terminal-height cap.",
            )?;
        }
        "command" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            press(&mut harness, crossterm::event::KeyCode::Char(':'))?;
            for _ in 0..8 {
                press(&mut harness, crossterm::event::KeyCode::Down)?;
            }
        }
        "composer" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            press(&mut harness, crossterm::event::KeyCode::Char('i'))?;
            type_into(
                &mut harness,
                "Explain this change carefully. The sticky composer expands, wraps, and remains independently scrollable.",
            )?;
            harness.key(KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::SHIFT,
            ))?;
            type_into(&mut harness, "Second editable line with a visible cursor →")?;
        }
        "quiet" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            harness.state.agent_connected = true;
            press(&mut harness, crossterm::event::KeyCode::Char('i'))?;
            type_into(&mut harness, "Audit authentication handling")?;
            press(&mut harness, crossterm::event::KeyCode::Enter)?;
            harness.inject_activity(
                ActivityKind::ToolStart,
                "Running security-review skill",
                Some("skill".into()),
                Some("security-review".into()),
            )?;
            harness.backdate_agent_progress(std::time::Duration::from_secs(23));
        }
        "queue" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            for prompt in [
                "Review the error path first",
                "Then check cancellation cleanup",
                "Finally summarize the public API",
            ] {
                press(&mut harness, crossterm::event::KeyCode::Char('i'))?;
                type_into(&mut harness, prompt)?;
                press(&mut harness, crossterm::event::KeyCode::Enter)?;
            }
            press(&mut harness, crossterm::event::KeyCode::Char(':'))?;
            type_into(&mut harness, "queue")?;
            press(&mut harness, crossterm::event::KeyCode::Enter)?;
        }
        "side" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            press(&mut harness, crossterm::event::KeyCode::Char('i'))?;
            type_into(&mut harness, "/side What does this function guarantee?")?;
            press(&mut harness, crossterm::event::KeyCode::Enter)?;
            harness.inject_side_started("main-session", "side-session")?;
        }
        "model" => {
            press(&mut harness, crossterm::event::KeyCode::Char(':'))?;
            type_into(&mut harness, "model")?;
            press(&mut harness, crossterm::event::KeyCode::Enter)?;
            harness.inject_agent_event(AgentEvent::ModelsListed(vec![
                ModelOption {
                    id: "fast".into(),
                    name: "Fast".into(),
                    supported_reasoning_efforts: vec!["low".into(), "medium".into()],
                    default_reasoning_effort: Some("medium".into()),
                    max_context_tokens: Some(32_768),
                    context_tiers: vec![ContextTierOption {
                        id: "default".into(),
                        max_context_tokens: Some(32_768),
                    }],
                },
                ModelOption {
                    id: "deep".into(),
                    name: "Deep".into(),
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
            ]))?;
        }
        "settings" => {
            press(&mut harness, crossterm::event::KeyCode::Char(':'))?;
            type_into(&mut harness, "settings")?;
            press(&mut harness, crossterm::event::KeyCode::Enter)?;
            harness.state.cache_directory = harness.paths.prs.display().to_string();
            harness.state.storage_path = harness.paths.database.display().to_string();
            harness.state.skill_directories = harness.paths.skills.display().to_string();
            harness.state.settings_index = 9;
        }
        "markdown" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            harness.inject_agent_event(AgentEvent::HistoryLoaded(vec![HistoryEntry {
                role: "copilot".into(),
                text: "# Review result\n\n- **Safe** path\n- [Guide](https://example.invalid/review)\n\n| Check | Status |\n| :--- | ---: |\n| `retry` | bounded |\n\n> > Nested context\n\n```rust\nfn review() -> Result<()> {\n    Ok(())\n}\n```".into(),
            }]))?;
        }
        "tiny" => {
            harness.resize(width.min(20), height.min(5));
        }
        other => anyhow::bail!(
            "unknown UI snapshot state {other:?}; use review, ask, command, composer, quiet, queue, side, model, settings, markdown, tiny, or all"
        ),
    }
    harness.render()
}

/// Named diff fixtures for [`run_ui_script`], covering shapes that surface
/// different corner cases (long unchanged regions, many files, long lines,
/// wide/combining Unicode).
fn ui_script_fixture(name: &str) -> Result<(String, String)> {
    let diff = match name {
        "default" => concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,3 +1,4 @@\n",
            " fn review() {\n",
            "+    let visible = true;\n",
            "     finish();\n",
            " }\n",
        )
        .to_owned(),
        "foldheavy" => {
            let mut body = String::new();
            body.push_str("diff --git a/src/big.rs b/src/big.rs\n");
            body.push_str("--- a/src/big.rs\n");
            body.push_str("+++ b/src/big.rs\n");
            body.push_str("@@ -1,80 +1,81 @@\n");
            for line_number in 1..=30 {
                body.push_str(&format!(" fn helper_{line_number}() {{}}\n"));
            }
            body.push_str("+    let inserted_marker = true;\n");
            for line_number in 31..=80 {
                body.push_str(&format!(" fn helper_{line_number}() {{}}\n"));
            }
            body
        }
        "manyfiles" => {
            let mut body = String::new();
            for index in 1..=12 {
                body.push_str(&format!(
                    "diff --git a/src/module_{index}.rs b/src/module_{index}.rs\n"
                ));
                body.push_str(&format!("--- a/src/module_{index}.rs\n"));
                body.push_str(&format!("+++ b/src/module_{index}.rs\n"));
                body.push_str("@@ -1,2 +1,3 @@\n");
                body.push_str(" fn existing() {}\n");
                body.push_str(&format!("+fn added_{index}() {{}}\n"));
                body.push_str(" fn trailer() {}\n");
            }
            body
        }
        "longlines" => {
            let long_old = "x".repeat(40) + &" old_token".repeat(30);
            let long_new = "y".repeat(40) + &" new_token_replacement_value".repeat(30);
            format!(
                "diff --git a/src/long.rs b/src/long.rs\n--- a/src/long.rs\n+++ b/src/long.rs\n@@ -1,3 +1,3 @@\n fn wrap() {{\n-    let value = \"{long_old}\";\n+    let value = \"{long_new}\";\n }}\n"
            )
        }
        "unicode" => concat!(
            "diff --git a/src/unicode.rs b/src/unicode.rs\n",
            "--- a/src/unicode.rs\n",
            "+++ b/src/unicode.rs\n",
            "@@ -1,3 +1,4 @@\n",
            " fn greet() {\n",
            "+    // 你好世界 こんにちは мир 🎉🚀👍 café naïve e\u{301}\n",
            "     finish();\n",
            " }\n",
        )
        .to_owned(),
        other => anyhow::bail!(
            "unknown ui-script fixture {other:?}; use default, foldheavy, manyfiles, longlines, or unicode"
        ),
    };
    Ok((name.to_owned(), diff))
}

fn parse_key_spec(spec: &str) -> Result<KeyEvent> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = spec;
    loop {
        if let Some(tail) = rest.strip_prefix("C-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("S-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("A-") {
            modifiers |= KeyModifiers::ALT;
            rest = tail;
        } else {
            break;
        }
    }
    let code = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        _ => {
            let mut chars = rest.chars();
            let first = chars
                .next()
                .with_context(|| format!("empty key spec {spec:?}"))?;
            if chars.next().is_some() {
                anyhow::bail!("key spec {spec:?} must name one key (named key or single char)");
            }
            KeyCode::Char(first)
        }
    };
    Ok(KeyEvent::new(code, modifiers))
}

fn decode_script_text(text: &str) -> Result<String> {
    let mut decoded = String::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        let escaped = characters
            .next()
            .context("trailing backslash in scripted text")?;
        decoded.push(match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '\\' => '\\',
            other => anyhow::bail!("unsupported scripted escape \\{other}"),
        });
    }
    Ok(decoded)
}

/// Runs a plain-text action script against a named diff fixture and returns
/// every requested snapshot plus a final state/debug dump. Intended as the
/// entry point subagents drive over `rq-tui ui-script` without needing to
/// write Rust or recompile the binary for each new scenario.
///
/// Script grammar, one action per line (blank lines and `#`-comments ignored):
///   key <spec>       press one key; spec is `[C-][S-][A-]<name-or-char>`,
///                     e.g. `a`, `Enter`, `C-w`, `S-Tab`, `C-c`
///   type <text>       press every remaining character on the line in order
///   paste <text>      inject one terminal paste event without submitting
///   mouse <up|down>   inject one three-row mouse-wheel event
///   resize <w> <h>    change terminal dimensions
///   stream <text>     deliver and complete a single-chunk fake-agent response
///   stream-start [main|side] <text>
///                     start a response on the visible or explicit lane
///   stream-delta <text> append a delta to the active response
///   stream-complete   complete the active response
///   stream-abort      abort the active response
///   fail <message>    fail the oldest queued response before it starts
///   history <role> <text> load a one-entry persisted-history snapshot
///   history-append <role> <text> append one deterministic transcript entry
///   activity <kind> <label> inject intent/reasoning/tool-start/tool-progress/
///                     tool-complete/retry/failure/other durable SDK activity
///   models            inject deterministic model capabilities
///   quiet <seconds>   backdate active progress for quiet/warning rendering
///   disconnect <text> inject a visible SDK disconnect/error
///   side-start        complete deterministic SIDE creation
///   side-fail <text>  fail deterministic SIDE creation
///   side-exit         complete deterministic SIDE teardown
///   snapshot [label]  render the current frame into the output now
pub fn run_ui_script(fixture: &str, width: u16, height: u16, script: &str) -> Result<String> {
    let _deterministic_ids = deterministic_outbound_ids();
    let (name, diff) = ui_script_fixture(fixture)?;
    let mut harness = TuiHarness::from_unified_diff(name, &diff, width, height)?;
    let mut output = String::new();
    let mut snapshot_count = 0usize;

    let mut emit_snapshot = |harness: &mut TuiHarness, label: &str, output: &mut String| {
        snapshot_count += 1;
        output.push_str(&format!(
            "\n===== snapshot {snapshot_count}: {label} ({}x{}, mode={}) =====\n",
            harness.width,
            harness.height,
            harness.mode()
        ));
        match harness.render() {
            Ok(frame) => output.push_str(&frame),
            Err(error) => output.push_str(&format!("<render error: {error:#}>\n")),
        }
        output.push_str(&format!("status: {}\n", harness.status()));
    };

    for (line_number, raw_line) in script.lines().enumerate() {
        let line = raw_line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (command, raw_argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = if matches!(
            command,
            "type"
                | "paste"
                | "stream"
                | "stream-start"
                | "stream-delta"
                | "fail"
                | "history"
                | "history-append"
                | "activity"
                | "disconnect"
        ) {
            raw_argument
        } else {
            raw_argument.trim()
        };
        match command {
            "key" => {
                let key = parse_key_spec(argument)
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
                harness
                    .key(key)
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "type" => {
                for character in argument.chars() {
                    harness
                        .key(KeyEvent::new(
                            crossterm::event::KeyCode::Char(character),
                            crossterm::event::KeyModifiers::NONE,
                        ))
                        .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
                }
            }
            "paste" => {
                let pasted = decode_script_text(argument)?;
                harness
                    .paste(&pasted)
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "mouse" => match argument {
                "up" => harness.mouse_scroll(true)?,
                "down" => harness.mouse_scroll(false)?,
                _ => anyhow::bail!(
                    "line {}: mouse requires up or down, got {argument:?}",
                    line_number + 1
                ),
            },
            "resize" => {
                let mut parts = argument.split_whitespace();
                let w: u16 = parts
                    .next()
                    .context("resize requires <w> <h>")?
                    .parse()
                    .context("resize width must be a number")?;
                let h: u16 = parts
                    .next()
                    .context("resize requires <w> <h>")?
                    .parse()
                    .context("resize height must be a number")?;
                harness.resize(w, h);
            }
            "stream" => {
                harness
                    .stream_next_response(&[argument])
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "stream-start" => {
                let result = if let Some(first_delta) = argument.strip_prefix("main ") {
                    harness.start_next_response_on_lane(AgentLane::Main, first_delta)
                } else if let Some(first_delta) = argument.strip_prefix("side ") {
                    let side_id = harness
                        .state
                        .side_session_id
                        .clone()
                        .context("stream-start side requires an active SIDE session")?;
                    harness.start_next_response_on_lane(
                        AgentLane::Side { id: side_id },
                        first_delta,
                    )
                } else {
                    harness.start_next_response(argument)
                };
                result
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "stream-delta" => {
                harness
                    .push_response_delta(argument)
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "stream-complete" => {
                harness
                    .complete_response(false)
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "stream-abort" => {
                harness
                    .complete_response(true)
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "fail" => {
                harness
                    .fail_next_response(argument)
                    .with_context(|| format!("line {}: {raw_line:?}", line_number + 1))?;
            }
            "history" => {
                let (role, text) = argument
                    .split_once(' ')
                    .context("history requires <role> <text>")?;
                harness.inject_agent_event(AgentEvent::HistoryLoaded(vec![HistoryEntry {
                    role: role.to_owned(),
                    text: text.to_owned(),
                }]))?;
            }
            "history-append" => {
                let (role, text) = argument
                    .split_once(' ')
                    .context("history-append requires <role> <text>")?;
                harness.append_history(role, text);
            }
            "activity" => {
                let (kind, label) = argument
                    .split_once(' ')
                    .context("activity requires <kind> <label>")?;
                let kind = match kind {
                    "intent" => ActivityKind::Intent,
                    "reasoning" => ActivityKind::Reasoning,
                    "tool-start" => ActivityKind::ToolStart,
                    "tool-progress" => ActivityKind::ToolProgress,
                    "tool-complete" => ActivityKind::ToolComplete,
                    "retry" => ActivityKind::Retry,
                    "failure" => ActivityKind::Failure,
                    "other" | "skill" | "subagent" => ActivityKind::Other,
                    _ => anyhow::bail!(
                        "activity kind must be intent, reasoning, tool-start, tool-progress, tool-complete, retry, failure, skill, subagent, or other"
                    ),
                };
                let tool = matches!(
                    kind,
                    ActivityKind::ToolStart
                        | ActivityKind::ToolProgress
                        | ActivityKind::ToolComplete
                        | ActivityKind::Failure
                )
                .then(|| "deterministic-tool".to_owned());
                harness.inject_activity(kind, label, tool, Some("ui-script injection".into()))?;
            }
            "models" => {
                harness.inject_agent_event(AgentEvent::ModelsListed(vec![ModelOption {
                    id: "script-model".into(),
                    name: "Script Model".into(),
                    supported_reasoning_efforts: vec!["low".into(), "high".into()],
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
                }]))?;
            }
            "quiet" => {
                let seconds = argument
                    .parse::<u64>()
                    .context("quiet requires an integer number of seconds")?;
                harness.backdate_agent_progress(std::time::Duration::from_secs(seconds));
            }
            "disconnect" => {
                harness.inject_agent_event(AgentEvent::Error(argument.to_owned()))?;
            }
            "side-start" => {
                harness.inject_side_started("script-main", "script-side")?;
            }
            "side-fail" => {
                harness.inject_side_failed(argument)?;
            }
            "side-exit" => {
                harness.inject_side_exited("script-main", "script-side")?;
            }
            "snapshot" => {
                let label = if argument.is_empty() {
                    format!("line {}", line_number + 1)
                } else {
                    argument.to_owned()
                };
                emit_snapshot(&mut harness, &label, &mut output);
            }
            other => anyhow::bail!(
                "line {}: unknown ui-script command {other:?} (use key, type, paste, mouse, resize, stream*, fail, history*, activity, models, quiet, disconnect, side-*, or snapshot)",
                line_number + 1
            ),
        }
        if command != "snapshot" {
            harness
                .draw()
                .map(|_| ())
                .with_context(|| format!("line {}: render after {raw_line:?}", line_number + 1))?;
        }
    }

    emit_snapshot(&mut harness, "final", &mut output);
    output.push_str(&format!("mode: {}\n", harness.mode()));
    output.push_str(&format!(
        "compose ({} bytes, cursor {}): {:?}\n",
        harness.compose_text().len(),
        harness.state.compose_cursor,
        harness.compose_text()
    ));
    output.push_str(&format!(
        "compose_scroll: {}\n",
        harness.state.compose_scroll
    ));
    output.push_str(&format!("review_scroll: {}\n", harness.state.review_scroll));
    output.push_str(&format!(
        "review_cursor: {} · composer_cursor_rows: {:?}\n",
        harness.state.review_cursor,
        harness
            .state
            .review_stream()
            .rows()
            .iter()
            .enumerate()
            .filter_map(|(index, row)| match row {
                crate::review_stream::ReviewRow::Annotation { block, .. }
                    if block.text.contains('▏') =>
                {
                    Some(index)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    ));
    output.push_str(&format!("chat_scroll: {}\n", harness.chat_scroll()));
    output.push_str(&format!("last_yank: {:?}\n", harness.last_yank()));
    output.push_str(&format!("effects: {:?}\n", harness.captured_effects()));
    output.push_str(&format!("agent_commands: {:?}\n", harness.agent_commands()));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::TuiHarness;
    use crate::app::tests_support::state_for_ui;
    use crate::app::{AgentPhase, Focus, Screen, VersionChoice};
    use crate::copilot::{
        ActivityKind, AgentEvent, AgentLane, ContextTierOption, HistoryEntry, ModelOption,
    };
    use crate::domain::{
        AnchorSide, Annotation, AnnotationKind, DeliveryState, Placement, Version, VersionKind,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(harness: &mut TuiHarness, text: &str) {
        for character in text.chars() {
            harness.key(key(KeyCode::Char(character))).unwrap();
        }
    }

    fn workflow_diff() -> &'static str {
        concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -10,3 +10,4 @@\n",
            " fn review() {\n",
            "+    let added = true;\n",
            "     use_added();\n",
            " }\n",
        )
    }

    #[test]
    fn harness_drives_keys_effects_storage_agent_and_frames_without_a_tty() {
        let mut harness = TuiHarness::new(state_for_ui(), 100, 24).unwrap();
        assert!(harness.render().unwrap().contains("demo — Review"));
        harness.key(key(KeyCode::Tab)).unwrap();
        assert!(harness.render().unwrap().contains("demo — Chat"));
    }

    #[test]
    fn public_fixture_constructor_parses_and_renders_a_diff() {
        let diff = "diff --git a/a.rs b/a.rs\n\
                    --- a/a.rs\n\
                    +++ b/a.rs\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    +new\n";
        let mut harness = TuiHarness::from_unified_diff("fixture", diff, 90, 20).unwrap();
        let frame = harness.render().unwrap();
        assert!(frame.contains("fixture — Review"));
        assert!(frame.contains("new"));
    }

    #[test]
    fn review_navigation_is_one_cross_file_semantic_stream() {
        let output = super::run_ui_script(
            "manyfiles",
            100,
            28,
            "key j\nkey j\nkey j\nkey j\nkey j\nsnapshot crossed\n",
        )
        .unwrap();
        assert!(output.contains("repo > src/module_1.rs"));
        assert!(output.contains("repo > src/module_2.rs  │  2/12 files"));
        assert!(output.contains("M repo > src/module_3.rs"));
    }

    #[test]
    fn compact_file_tree_keeps_the_selected_file_visible_with_change_counts() {
        let mut script = String::from("key t\n");
        for _ in 0..8 {
            script.push_str("key j\n");
        }
        script.push_str("snapshot selected\n");

        let output = super::run_ui_script("manyfiles", 40, 9, &script).unwrap();

        assert!(output.contains("src/module_9.rs"));
        assert!(output.contains("+1 -0"));
        assert!(output.contains("files · 8-10/13"));
    }

    #[test]
    fn file_tree_fuzzy_filter_selects_and_reveals_the_first_live_match() {
        let output = super::run_ui_script(
            "manyfiles",
            40,
            9,
            "key t\nkey /\ntype m12\nsnapshot filtered\nkey Enter\nsnapshot accepted\n",
        )
        .unwrap();

        assert!(output.contains("/m12"));
        assert!(output.contains("src/module_12.rs"));
        assert!(output.contains("status: File filter: m12"));
    }

    #[test]
    fn compact_version_history_keeps_the_selected_version_and_controls_visible() {
        let mut harness = TuiHarness::new(state_for_ui(), 40, 9).unwrap();
        harness.state.screen = Screen::Versions;
        harness.state.versions = (1..=12)
            .map(|number| VersionChoice {
                repo_name: format!("repo-{number}"),
                version: Version {
                    id: format!("version-{number}"),
                    repo_id: "repo".into(),
                    version_num: number,
                    kind: VersionKind::Remote,
                    created_at: number.to_string(),
                    head_sha: format!("head-{number}"),
                    worktree_path: None,
                    last_opened_at: (number < 12).then(|| "earlier".into()),
                },
                asks: number as usize,
                comments: 0,
            })
            .collect();
        harness.state.version_index = 11;

        let frame = harness.render().unwrap();

        assert!(frame.contains("repo-12"));
        assert!(frame.contains("Version History · 7-12/12"));
        assert!(frame.contains("Enter open · q/Esc back"));
    }

    #[test]
    fn inline_blocks_are_bordered_navigable_foldable_stream_rows() {
        let mut harness = TuiHarness::from_unified_diff("inline", workflow_diff(), 72, 20).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(
            &mut harness,
            "a deliberately long comment that wraps inside its border without overflowing",
        );
        harness.key(key(KeyCode::Enter)).unwrap();
        let saved = harness.render().unwrap();
        assert!(saved.contains("╭─ Comment · src/lib.rs R10"));
        assert!(saved.contains("╰────────────────"));

        harness.key(key(KeyCode::Char('j'))).unwrap();
        assert!(harness.render().unwrap().contains("╭─ ❯ Comment"));
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('z'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        let collapsed = harness.render().unwrap();
        assert!(collapsed.contains("▸ Comment · src/lib.rs R10"));
        assert!(!collapsed.contains("deliberately long comment"));
    }

    #[test]
    fn repeated_annotations_refocus_the_source_instead_of_a_prior_footer() {
        let mut harness =
            TuiHarness::from_unified_diff("repeat-inline", workflow_diff(), 72, 20).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "first");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "second");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert_eq!(harness.persisted_annotations().unwrap().len(), 2);
        harness.key(key(KeyCode::Char('c'))).unwrap();
        assert_eq!(harness.mode(), "INSERT");
        assert!(harness.status().is_empty() || !harness.status().contains("Cannot annotate"));
    }

    #[test]
    fn deleting_a_queued_ask_cancels_and_quarantines_its_late_response() {
        let mut harness =
            TuiHarness::from_unified_diff("delete-queued", workflow_diff(), 80, 20).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "delete before response");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char(']'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        harness.key(key(KeyCode::Char('d'))).unwrap();
        harness.key(key(KeyCode::Char('d'))).unwrap();

        assert!(harness.persisted_annotations().unwrap().is_empty());
        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.contains("CancelQueued")));
        harness.start_next_response("late answer").unwrap();
        assert!(harness.status().contains("deleted Ask"));
        harness.complete_response(false).unwrap();
        assert!(harness.render().is_ok());
        assert!(harness.selected_cell_count().unwrap() > 0);
    }

    #[test]
    fn visual_mode_cannot_start_on_review_chrome() {
        let mut harness =
            TuiHarness::from_unified_diff("header-visual", workflow_diff(), 80, 20).unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert!(harness.status().contains("source rows"));
        harness.key(key(KeyCode::Char('a'))).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert!(harness.status().contains("Cannot annotate"));
    }

    #[test]
    fn review_visual_top_and_bottom_stay_inside_the_current_file() {
        let mut harness = TuiHarness::from_unified_diff(
            "manyfiles",
            &super::ui_script_fixture("manyfiles").unwrap().1,
            90,
            20,
        )
        .unwrap();
        let first_file_lines = harness.state.current_line_count();
        harness.key(key(KeyCode::Char('V'))).unwrap();
        harness.key(key(KeyCode::Char('G'))).unwrap();
        assert_eq!(harness.mode(), "VISUAL");
        assert_eq!(harness.state.file_index, 0);
        assert_eq!(harness.state.cursor, first_file_lines.saturating_sub(1));
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        assert_eq!(harness.mode(), "VISUAL");
        assert_eq!(harness.state.file_index, 0);
        assert_eq!(harness.state.cursor, 0);
    }

    #[test]
    fn annotation_for_a_missing_file_stays_visible_in_the_review_stream() {
        let mut harness =
            TuiHarness::from_unified_diff("missing-annotation", workflow_diff(), 84, 22).unwrap();
        let repo_id = harness.state.work_item.repos[0].record.id.clone();
        let version_id = harness.state.work_item.repos[0].version.id.clone();
        harness.state.annotations.push((
            Annotation {
                id: "missing-file-note".into(),
                repo_id,
                kind: AnnotationKind::Comment,
                file_path: "src/removed.rs".into(),
                anchor_snippet: "removed source".into(),
                anchor_hash: "missing".into(),
                anchor_start_offset: 0,
                anchor_line_count: 1,
                text: Some("retain this historical note".into()),
                submitted: false,
                delivery_state: DeliveryState::Draft,
                created_at: "2026-07-30T00:00:00Z".into(),
            },
            Placement {
                annotation_id: "missing-file-note".into(),
                version_id,
                side: AnchorSide::New,
                line_start: 17,
                line_end: 17,
                outdated: true,
                ambiguous: false,
            },
        ));

        harness.key(key(KeyCode::Char(']'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        let frame = harness.render().unwrap();
        assert!(frame.contains("D"));
        assert!(frame.contains("src/removed.rs"));
        assert!(frame.contains("!Comment"));
        assert!(frame.contains("retain this historical note"));
    }

    #[test]
    fn i_on_an_ask_block_starts_a_new_follow_up_instead_of_editing_history() {
        let mut harness =
            TuiHarness::from_unified_diff("follow-up", workflow_diff(), 90, 22).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "original question");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.stream_next_response(&["first answer"]).unwrap();
        harness.key(key(KeyCode::Char(']'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        assert_eq!(harness.mode(), "INSERT");
        assert!(harness.compose_text().is_empty());
        type_text(&mut harness, "new follow up");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness
            .captured_effects()
            .iter()
            .any(|effect| effect.contains("FollowUpAsk") && effect.contains("new follow up")));
        assert!(!harness
            .captured_effects()
            .iter()
            .any(|effect| effect.contains("EditAskMessage")));
    }

    #[test]
    fn normal_mode_gm_previews_and_restores_a_preserved_inline_draft() {
        let mut harness =
            TuiHarness::from_unified_diff("draft-preview", workflow_diff(), 82, 21).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "# Draft\n\npreview **this**");
        harness.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert_eq!(harness.compose_text(), "# Draft\n\npreview **this**");
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('m'))).unwrap();
        let preview = harness.render().unwrap();
        assert!(preview.contains("Markdown preview"));
        assert!(preview.contains("Draft"));
        harness.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert_eq!(harness.compose_text(), "# Draft\n\npreview **this**");
    }

    #[test]
    fn queued_chat_prompts_can_be_edited_before_delivery() {
        let mut harness =
            TuiHarness::from_unified_diff("queue-edit", workflow_diff(), 84, 22).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "active prompt");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.start_next_response("working").unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "queued original");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "queue");
        harness.key(key(KeyCode::Enter)).unwrap();
        let original_id = harness.state.queue_entry_ids()[1].0.clone();
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Char('e'))).unwrap();
        assert_eq!(harness.mode(), "INSERT");
        assert_eq!(harness.compose_text(), "queued original");
        let edit_frame = harness.render().unwrap();
        assert!(edit_frame.contains("EDIT QUEUED"));
        assert!(edit_frame.contains("Enter replaces"));
        assert!(edit_frame.contains(&original_id[..8]));
        harness
            .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .unwrap();
        type_text(&mut harness, "queued replacement");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness
            .captured_effects()
            .iter()
            .any(|effect| effect.contains("ReplaceQueued")));
        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.contains("ReplaceQueued")));
        assert!(harness.status().contains("replaced atomically"));
        let pending = harness.agent.pending.lock().expect("agent lock");
        assert!(!pending.iter().any(|outbound| outbound.id == original_id));
        assert!(pending
            .iter()
            .any(|outbound| outbound.text == "queued replacement"));
    }

    #[test]
    fn queue_edit_rejection_never_enqueues_a_duplicate_replacement() {
        let mut harness =
            TuiHarness::from_unified_diff("queue-race", workflow_diff(), 84, 22).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "active prompt");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.start_next_response("working").unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "queued original");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "queue");
        harness.key(key(KeyCode::Enter)).unwrap();
        let original_id = harness.state.queue_entry_ids()[1].0.clone();
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Char('e'))).unwrap();
        harness
            .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .unwrap();
        type_text(&mut harness, "must not run");

        harness
            .agent
            .pending
            .lock()
            .expect("agent lock")
            .retain(|outbound| outbound.id != original_id);
        harness.key(key(KeyCode::Enter)).unwrap();

        assert!(harness.status().contains("rejected"));
        assert!(!harness.state.pending_outbound_ids.contains(&original_id));
        assert!(!harness
            .agent
            .pending
            .lock()
            .expect("agent lock")
            .iter()
            .any(|outbound| outbound.text == "must not run"));
        assert!(harness
            .state
            .chat
            .iter()
            .find(|entry| entry.text == "must not run")
            .and_then(|entry| entry.error.as_deref())
            .is_some_and(|error| error.contains("rejected")));
    }

    #[test]
    fn editing_a_parked_main_queue_entry_from_side_keeps_it_on_main() {
        let mut harness =
            TuiHarness::from_unified_diff("side-main-edit", workflow_diff(), 92, 22).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        for prompt in ["main first", "main queued original"] {
            harness.key(key(KeyCode::Char('i'))).unwrap();
            type_text(&mut harness, prompt);
            harness.key(key(KeyCode::Enter)).unwrap();
        }
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();

        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "queue");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Char('e'))).unwrap();
        harness
            .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .unwrap();
        type_text(&mut harness, "main queued replacement");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(!harness
            .state
            .chat
            .iter()
            .any(|entry| entry.text == "main queued replacement"));
        assert!(harness.state.main_chat.as_ref().is_some_and(|chat| chat
            .iter()
            .any(|entry| entry.text == "main queued replacement")));

        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/main");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(!harness.state.side_active);
        assert!(harness
            .state
            .chat
            .iter()
            .any(|entry| entry.text == "main queued replacement"));
        assert!(harness.status().contains("MAIN restored"));
    }

    #[test]
    fn ui_script_exposes_models_activity_quiet_history_and_exact_effects() {
        let output = super::run_ui_script(
            "unicode",
            84,
            22,
            "key Tab\n\
             history assistant **restored** `history`\n\
             activity tool-start searching repository\n\
             quiet 30\n\
             snapshot activity\n\
             key :\n\
             type model\n\
             key Enter\n\
             models\n\
             snapshot models\n",
        )
        .unwrap();
        assert!(output.contains("restored"));
        assert!(output.contains("searching repository"));
        assert!(output.contains("Script Model"));
        assert!(output.contains("last_yank:"));
        assert!(output.contains("effects:"));
    }

    #[test]
    fn ui_script_supports_paste_mouse_and_appendable_history() {
        let output = super::run_ui_script(
            "default",
            80,
            20,
            "key Tab\n\
             key i\n\
             paste first pasted line\\nsecond pasted line\n\
             snapshot pasted\n\
             key C-c\n\
             history-append assistant first restored response\n\
             history-append assistant second restored response\n\
             mouse up\n\
             snapshot history\n",
        )
        .unwrap();

        assert!(output.contains("first pasted line"));
        assert!(output.contains("second pasted line"));
        assert!(output.contains("first restored response"));
        assert!(output.contains("second restored response"));
        assert!(output.contains("Pasted 36 bytes across 2 lines"));
    }

    #[test]
    fn ui_snapshots_are_byte_deterministic_across_independent_runs() {
        let first_review = super::render_ui_scenario("review", 80, 18).unwrap();
        let second_review = super::render_ui_scenario("review", 80, 18).unwrap();
        assert_eq!(first_review, second_review);
        assert!(first_review.contains("CONNECTING 0ms"));

        let first = super::render_ui_scenario("queue", 100, 28).unwrap();
        let second = super::render_ui_scenario("queue", 100, 28).unwrap();
        assert_eq!(first, second);
        assert!(first.contains("00000001"));
        assert!(first.contains("00000002"));
    }

    #[test]
    fn normal_line_ask_persists_queues_streams_and_completes_inline() {
        let mut harness =
            TuiHarness::from_unified_diff("workflow", workflow_diff(), 110, 28).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        assert_eq!(harness.mode(), "INSERT");
        assert!(harness.render().unwrap().contains("Ask"));
        type_text(&mut harness, "Why is this needed?");
        harness.key(key(KeyCode::Enter)).unwrap();

        let annotations = harness.persisted_annotations().unwrap();
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].kind, "ask");
        assert_eq!(
            (annotations[0].line_start, annotations[0].line_end),
            (10, 10)
        );
        assert_eq!(harness.pending_outbound_count(), 1);
        assert_eq!(harness.agent_commands().len(), 1);
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff unified");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness.render().unwrap().contains("Why is this needed?"));

        harness
            .stream_next_response(&["Because ", "the caller requires it."])
            .unwrap();
        let annotations = harness.persisted_annotations().unwrap();
        assert_eq!(annotations[0].delivery_state, "sent");
        assert_eq!(harness.pending_outbound_count(), 0);
        assert!(harness
            .render()
            .unwrap()
            .contains("the caller requires it."));
    }

    #[test]
    fn visual_range_ask_uses_exact_new_source_range() {
        let mut harness =
            TuiHarness::from_unified_diff("workflow", workflow_diff(), 110, 28).unwrap();
        harness.key(key(KeyCode::Char('V'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        assert_eq!(harness.mode(), "VISUAL");
        assert!(harness.selected_cell_count().unwrap() > 220);
        harness.key(key(KeyCode::Char('a'))).unwrap();
        assert!(harness.render().unwrap().contains("new lines 10-12"));
        type_text(&mut harness, "Explain this range");
        harness.key(key(KeyCode::Enter)).unwrap();

        let annotation = harness.persisted_annotations().unwrap().remove(0);
        assert_eq!(annotation.side, "new");
        assert_eq!((annotation.line_start, annotation.line_end), (10, 12));
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff unified");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .stream_next_response(&["Range ", "explained"])
            .unwrap();
        assert!(harness.render().unwrap().contains("explained"));
    }

    #[test]
    fn normal_and_visual_comments_persist_without_contacting_agent() {
        let mut normal = TuiHarness::from_unified_diff("normal", workflow_diff(), 110, 28).unwrap();
        normal.key(key(KeyCode::Char('j'))).unwrap();
        normal.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut normal, "Keep this local");
        normal.key(key(KeyCode::Enter)).unwrap();
        let annotation = normal.persisted_annotations().unwrap().remove(0);
        assert_eq!(annotation.kind, "comment");
        assert_eq!((annotation.line_start, annotation.line_end), (11, 11));
        assert_eq!(annotation.text.as_deref(), Some("Keep this local"));
        assert!(normal.agent_commands().is_empty());
        assert!(normal.render().unwrap().contains("Keep this local"));
        normal.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut normal, "diff split");
        normal.key(key(KeyCode::Enter)).unwrap();
        let split = normal.render().unwrap();
        assert!(split.contains("Comment · src/lib.rs"));
        assert!(split.contains("Keep this local"));
        assert!(!split.contains("ask / comments (this file)"));

        let mut visual = TuiHarness::from_unified_diff("visual", workflow_diff(), 110, 28).unwrap();
        visual.key(key(KeyCode::Char('j'))).unwrap();
        visual.key(key(KeyCode::Char('V'))).unwrap();
        visual.key(key(KeyCode::Char('j'))).unwrap();
        visual.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut visual, "Two-line note");
        visual.key(key(KeyCode::Enter)).unwrap();
        let annotation = visual.persisted_annotations().unwrap().remove(0);
        assert_eq!(annotation.side, "new");
        assert_eq!((annotation.line_start, annotation.line_end), (11, 12));
        assert!(visual.agent_commands().is_empty());
    }

    #[test]
    fn visual_yank_preserves_source_order_across_addition_and_context() {
        let mut harness = TuiHarness::from_unified_diff("yank", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Char('V'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(
            harness.last_yank(),
            Some("fn review() {\n    let added = true;\n    use_added();\n")
        );
        assert_eq!(harness.mode(), "NORMAL");
    }

    #[test]
    fn folds_and_mixed_old_new_ranges_fail_safely() {
        let folded = concat!(
            "diff --git a/a.rs b/a.rs\n",
            "--- a/a.rs\n",
            "+++ b/a.rs\n",
            "@@ -1 +1 @@\n",
            " one\n",
            "@@ -10 +10 @@\n",
            " ten\n",
        );
        let mut fold = TuiHarness::from_unified_diff("fold", folded, 90, 20).unwrap();
        fold.key(key(KeyCode::Char('j'))).unwrap();
        fold.key(key(KeyCode::Char('c'))).unwrap();
        assert_eq!(fold.mode(), "NORMAL");
        assert!(fold.status().contains("fold and metadata"));
        assert!(fold.persisted_annotations().unwrap().is_empty());

        let replacement = "diff --git a/a.rs b/a.rs\n\
                           --- a/a.rs\n\
                           +++ b/a.rs\n\
                           @@ -7 +7 @@\n\
                           -old\n\
                           +new\n";
        let mut mixed = TuiHarness::from_unified_diff("mixed", replacement, 90, 20).unwrap();
        mixed.key(key(KeyCode::Char('v'))).unwrap();
        mixed.key(key(KeyCode::Char('j'))).unwrap();
        mixed.key(key(KeyCode::Char('a'))).unwrap();
        assert_eq!(mixed.mode(), "VISUAL");
        assert!(mixed
            .status()
            .contains("either old/deleted lines or new/added"));
    }

    #[test]
    fn deleted_line_annotation_records_the_old_side() {
        let deleted = "diff --git a/a.rs b/a.rs\n\
                       --- a/a.rs\n\
                       +++ b/a.rs\n\
                       @@ -7 +7,0 @@\n\
                       -removed();\n";
        let mut harness = TuiHarness::from_unified_diff("deleted", deleted, 90, 20).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "Why remove this?");
        harness.key(key(KeyCode::Enter)).unwrap();
        let annotation = harness.persisted_annotations().unwrap().remove(0);
        assert_eq!(annotation.side, "old");
        assert_eq!((annotation.line_start, annotation.line_end), (7, 7));
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff unified");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness.render().unwrap().contains("Why remove this?"));
    }

    #[test]
    fn cancel_restores_visual_selection_without_stale_draft() {
        for trigger in ['a', 'c'] {
            let mut harness =
                TuiHarness::from_unified_diff("cancel", workflow_diff(), 100, 24).unwrap();
            harness.key(key(KeyCode::Char('v'))).unwrap();
            harness.key(key(KeyCode::Char('j'))).unwrap();
            harness.key(key(KeyCode::Char(trigger))).unwrap();
            type_text(&mut harness, "discard me");
            harness.key(key(KeyCode::Esc)).unwrap();
            assert_eq!(harness.mode(), "VISUAL");
            assert!(!harness.render().unwrap().contains("discard me"));
            assert!(harness.selected_cell_count().unwrap() > 0);
            assert!(harness.persisted_annotations().unwrap().is_empty());
        }
    }

    #[test]
    fn failed_persistence_restores_the_contextual_composer_and_draft() {
        let mut harness =
            TuiHarness::from_unified_diff("failure", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "do not lose this");
        harness.set_storage_read_only(true).unwrap();
        harness.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(harness.mode(), "INSERT");
        assert!(harness.status().contains("Action failed"));
        assert!(harness.render().unwrap().contains("do not lose this"));
        assert!(harness.persisted_annotations().unwrap().is_empty());

        harness.set_storage_read_only(false).unwrap();
        harness.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(harness.persisted_annotations().unwrap().len(), 1);
    }

    #[test]
    fn failed_agent_delivery_is_pending_and_restart_requires_recovery_choice() {
        let mut harness =
            TuiHarness::from_unified_diff("recovery", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "Will this send?");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.fail_next_response("offline").unwrap();
        assert!(harness.status().contains("offline"));
        assert_eq!(
            harness.persisted_annotations().unwrap()[0].delivery_state,
            "pending"
        );

        harness.restart().unwrap();
        let frame = harness.render().unwrap();
        assert!(frame.contains("Delivery recovery"));
        assert!(frame.contains("resend"));
    }

    #[test]
    fn queued_chat_survives_restart_and_requires_an_explicit_recovery_choice() {
        let mut harness =
            TuiHarness::from_unified_diff("chat-recovery", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "durable queued chat");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(
            harness
                .storage
                .pending_chats(&harness.state.work_item.item.id)
                .unwrap()
                .len(),
            1
        );

        harness.restart().unwrap();
        let recovery = harness.render().unwrap();
        assert!(recovery.contains("Delivery recovery"));
        assert!(recovery.contains("queued Chat"));
        assert!(recovery.contains("durable queued chat"));
        assert!(harness.agent.pending.lock().expect("agent lock").is_empty());

        harness.key(key(KeyCode::Char('r'))).unwrap();
        assert_eq!(harness.state.screen, crate::app::Screen::Review);
        assert_eq!(harness.agent.pending.lock().expect("agent lock").len(), 1);
        harness.stream_next_response(&["recovered answer"]).unwrap();
        assert!(harness
            .storage
            .pending_chats(&harness.state.work_item.item.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn file_switch_clears_visual_mode_and_layout_changes_preserve_selection() {
        let two_files = format!(
            "{}{}",
            workflow_diff(),
            concat!(
                "diff --git a/src/two.rs b/src/two.rs\n",
                "--- a/src/two.rs\n",
                "+++ b/src/two.rs\n",
                "@@ -1 +1,2 @@\n",
                " one\n",
                "+two\n",
            )
        );
        let mut harness = TuiHarness::from_unified_diff("switch", &two_files, 110, 28).unwrap();
        harness.key(key(KeyCode::Char('V'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        let split_selected = harness.selected_cell_count().unwrap();
        assert!(split_selected > 110);

        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff unified");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(harness.mode(), "VISUAL");
        assert!(harness.render().unwrap().contains("VISUAL LINE"));
        assert!(harness.selected_cell_count().unwrap() > 110);

        harness.key(key(KeyCode::Char('l'))).unwrap();
        assert_eq!(harness.mode(), "VISUAL");
        assert!(harness.render().unwrap().contains("src/lib.rs"));
        harness.key(key(KeyCode::Esc)).unwrap();
        harness.key(key(KeyCode::Char('l'))).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert!(harness.render().unwrap().contains("src/two.rs"));
    }

    #[test]
    fn review_visual_modes_have_distinct_rendering_and_exact_copy_semantics() {
        let mut character =
            TuiHarness::from_unified_diff("review-char", workflow_diff(), 90, 20).unwrap();
        character.key(key(KeyCode::Char('v'))).unwrap();
        character.key(key(KeyCode::Char('l'))).unwrap();
        character.key(key(KeyCode::Char('l'))).unwrap();
        assert!(character.render().unwrap().contains("VISUAL CHAR"));
        character.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(character.last_yank(), Some("fn "));

        let mut line =
            TuiHarness::from_unified_diff("review-line", workflow_diff(), 90, 20).unwrap();
        line.key(key(KeyCode::Char('V'))).unwrap();
        line.key(key(KeyCode::Char('j'))).unwrap();
        assert!(line.render().unwrap().contains("VISUAL LINE"));
        line.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(
            line.last_yank(),
            Some("fn review() {\n    let added = true;\n")
        );

        let mut block =
            TuiHarness::from_unified_diff("review-block", workflow_diff(), 90, 20).unwrap();
        block
            .key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .unwrap();
        block.key(key(KeyCode::Char('l'))).unwrap();
        block.key(key(KeyCode::Char('j'))).unwrap();
        assert!(block.render().unwrap().contains("VISUAL BLOCK"));
        block.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(block.last_yank(), Some("fn\n  "));

        let mut compact =
            TuiHarness::from_unified_diff("review-compact-visual", workflow_diff(), 40, 9).unwrap();
        compact.key(key(KeyCode::Char('v'))).unwrap();
        let compact_frame = compact.render().unwrap();
        assert!(compact_frame.contains("VISUAL CHAR"));
        assert!(compact_frame.contains("y copy · Esc clear"));
    }

    #[test]
    fn review_character_selection_moves_and_copies_extended_graphemes_atomically() {
        let diff = "diff --git a/unicode.rs b/unicode.rs\n\
                    --- a/unicode.rs\n\
                    +++ b/unicode.rs\n\
                    @@ -0,0 +1 @@\n\
                    +e\u{301}👩\u{200d}💻漢字\n";
        let mut harness = TuiHarness::from_unified_diff("review-unicode", diff, 80, 18).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('l'))).unwrap();
        harness.key(key(KeyCode::Char('y'))).unwrap();

        assert_eq!(harness.last_yank(), Some("e\u{301}👩\u{200d}💻"));
    }

    #[test]
    fn review_character_selection_projects_to_full_annotation_lines() {
        let mut harness =
            TuiHarness::from_unified_diff("review-annotation", workflow_diff(), 90, 20).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('l'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "character selection comment");
        harness.key(key(KeyCode::Enter)).unwrap();

        let annotation = harness.persisted_annotations().unwrap().remove(0);
        assert_eq!((annotation.line_start, annotation.line_end), (10, 11));
        assert_eq!(annotation.side, "new");
        assert_eq!(harness.mode(), "NORMAL");
    }

    #[test]
    fn review_visual_cancel_restores_the_exact_character_range() {
        let mut harness =
            TuiHarness::from_unified_diff("review-cancel-range", workflow_diff(), 90, 20).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('l'))).unwrap();
        harness.key(key(KeyCode::Char('l'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "discard this draft");
        harness.key(key(KeyCode::Esc)).unwrap();

        assert_eq!(harness.mode(), "VISUAL");
        let frame = harness.render().unwrap();
        assert!(frame.contains("VISUAL CHAR · rows 1-2 · cols 1-3"));
        assert!(!frame.contains("discard this draft"));
        harness.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(harness.last_yank(), Some("fn review() {\n   "));
    }

    #[test]
    fn split_visual_selection_paints_only_the_annotation_side() {
        let mut harness =
            TuiHarness::from_unified_diff("review-side", workflow_diff(), 100, 20).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff split");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char('V'))).unwrap();

        assert!(harness.render().unwrap().contains("side new"));
        assert_eq!(harness.selected_cell_count_in_columns(0, 50).unwrap(), 0);
        assert!(harness.selected_cell_count_in_columns(50, 100).unwrap() > 0);
    }

    #[test]
    fn long_review_selection_pans_to_semantic_end_and_back() {
        let source = "let value = \"this source line is intentionally much wider than the review pane and its hidden tail is END\";";
        let diff = format!(
            "diff --git a/src/long.rs b/src/long.rs\n\
             --- a/src/long.rs\n\
             +++ b/src/long.rs\n\
             @@ -0,0 +1 @@\n\
             +{source}\n"
        );
        let mut harness = TuiHarness::from_unified_diff("review-long-pan", &diff, 58, 14).unwrap();
        harness.render().unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('$'))).unwrap();
        let end = harness.render().unwrap();
        assert!(end.contains("END"), "{end}");
        assert!(end.contains('‹'));
        assert!(harness.state.review_horizontal_scroll > 0);

        harness.key(key(KeyCode::Char('0'))).unwrap();
        let start = harness.render().unwrap();
        assert!(start.contains("let value"), "{start}");
        assert!(start.contains('…'));
        harness.key(key(KeyCode::Char('$'))).unwrap();
        harness.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(harness.last_yank(), Some(source));
    }

    #[test]
    fn narrow_terminal_keeps_selection_composer_and_top_palette_visible() {
        let mut harness = TuiHarness::from_unified_diff("narrow", workflow_diff(), 42, 12).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "narrow draft");
        let frame = harness.render().unwrap();
        assert!(frame.contains("Ask"));
        assert!(frame.contains("narrow draft"));

        harness.key(key(KeyCode::Esc)).unwrap();
        harness.key(key(KeyCode::Esc)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff");
        let frame = harness.render().unwrap();
        assert!(frame.contains("Command palette"));
        assert!(frame.contains(":diff"));
    }

    #[test]
    fn exact_minimum_pins_inline_editor_identity_cursor_and_controls() {
        let mut harness =
            TuiHarness::from_unified_diff("minimum-editor", workflow_diff(), 40, 9).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(
            &mut harness,
            "How does a long contextual composer fit while keeping its cursor visible?",
        );
        let frame = harness.render().unwrap();
        assert!(frame.contains("Ask · INSERT"));
        assert!(frame.contains('▏'));
        assert!(frame.contains("Enter"));
        assert!(frame.contains("Esc keep"));
        assert!(frame.contains("^C discard"));
    }

    #[test]
    fn exact_minimum_chat_keeps_the_sticky_composer_border_intact() {
        let mut harness =
            TuiHarness::from_unified_diff("minimum-chat", workflow_diff(), 40, 9).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(
            &mut harness,
            "a long chat draft that wraps and scrolls at the supported minimum",
        );

        let frame = harness.render().unwrap();
        assert!(frame.contains("INSERT"));
        assert!(frame.contains("Enter send"));
        assert!(frame.contains("Esc keep"));
        let bottom = frame.lines().last().unwrap_or_default();
        assert!(bottom.starts_with('└'), "{frame}");
        assert!(bottom.ends_with('┘'), "{frame}");
    }

    #[test]
    fn composers_never_wrap_inside_flags_keycaps_or_zwj_families() {
        let text = "clusters 🇺🇸 1\u{fe0f}\u{20e3} 👨\u{200d}👩\u{200d}👧\u{200d}👦";
        let mut chat =
            TuiHarness::from_unified_diff("chat-graphemes", workflow_diff(), 40, 12).unwrap();
        chat.key(key(KeyCode::Tab)).unwrap();
        chat.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut chat, text);
        let chat_frame = chat.render().unwrap();
        assert!(chat_frame.contains("🇺🇸"));
        assert!(chat_frame.contains("1\u{fe0f}\u{20e3}"));
        assert!(chat_frame.contains("👨\u{200d}👩\u{200d}👧\u{200d}👦"));

        let mut review =
            TuiHarness::from_unified_diff("review-graphemes", workflow_diff(), 42, 12).unwrap();
        review.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut review, text);
        let review_frame = review.render().unwrap();
        assert!(review_frame.contains("🇺🇸"));
        assert!(review_frame.contains("1\u{fe0f}\u{20e3}"));
        assert!(review_frame.contains("👨\u{200d}👩\u{200d}👧\u{200d}👦"));
    }

    #[test]
    fn narrow_file_picker_uses_the_full_body_width() {
        let mut harness =
            TuiHarness::from_unified_diff("narrow-files", workflow_diff(), 40, 9).unwrap();
        harness.key(key(KeyCode::Char('t'))).unwrap();
        let frame = harness.render().unwrap();
        assert!(frame.contains("▶ files"));
        assert!(frame.contains("src/lib.rs"));
        assert!(!frame.contains("unified"));
    }

    #[test]
    fn compact_inline_composer_keeps_a_scrollable_rectangle() {
        let mut harness =
            TuiHarness::from_unified_diff("compact-composer", workflow_diff(), 40, 9).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(
            &mut harness,
            "How does this overflow-resistant editor keep all of this content reachable?",
        );
        let frame = harness.render().unwrap();

        assert!(frame.contains("╭─ ▶ Ask · INSERT"), "{frame}");
        assert!(frame.contains("╰─ ↑↓ · Enter"), "{frame}");
        assert!(frame.contains('/'), "{frame}");
        assert_eq!(harness.state.compose_wrap_width, 37);
        assert!(harness.state.compose_scroll > 0);
    }

    #[test]
    fn long_diff_lines_show_a_clipping_marker_in_both_layouts() {
        let diff = "diff --git a/src/long.rs b/src/long.rs\n\
                    index 1111111..2222222 100644\n\
                    --- a/src/long.rs\n\
                    +++ b/src/long.rs\n\
                    @@ -1 +1 @@\n\
                    -let value = \"short\";\n\
                    +let value = \"this source line is intentionally much wider than either review pane and its hidden tail is END\";\n";
        let mut harness = TuiHarness::from_unified_diff("clipped", diff, 80, 14).unwrap();
        harness.key(key(KeyCode::Char('t'))).unwrap();

        let unified = harness.render().unwrap();
        assert!(unified.contains("let value"));
        assert!(unified.contains('…'));
        assert!(!unified.contains("END"));

        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff split");
        harness.key(key(KeyCode::Enter)).unwrap();
        let split = harness.render().unwrap();
        assert!(split.contains("let value"));
        assert!(split.contains('…'));
        assert!(!split.contains("END"));
    }

    #[test]
    fn quit_guard_survives_async_agent_progress_until_dismissed() {
        let mut harness =
            TuiHarness::from_unified_diff("quit-guard", workflow_diff(), 80, 18).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "unsubmitted local comment");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "q");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert!(harness.state.quit_guard.is_some());
        harness
            .inject_agent_event(AgentEvent::Activity {
                outbound_id: None,
                label: "background SDK progress that changes ordinary status".into(),
            })
            .unwrap();
        let frame = harness.render().unwrap();
        assert!(frame.contains("QUIT BLOCKED"));
        assert!(!harness.state.should_quit);

        harness.key(key(KeyCode::Esc)).unwrap();
        assert!(harness.state.quit_guard.is_none());
        assert!(harness.status().contains("warning dismissed"));
    }

    #[test]
    fn cancellation_before_response_start_leaves_a_transcript_marker() {
        let mut harness =
            TuiHarness::from_unified_diff("early-cancel", workflow_diff(), 80, 18).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "cancel this before any response");
        harness.key(key(KeyCode::Enter)).unwrap();
        let outbound_id = harness.state.chat[0].outbound_id.clone().unwrap();

        harness
            .inject_agent_event(AgentEvent::ResponseComplete {
                outbound_id,
                aborted: true,
            })
            .unwrap();

        let frame = harness.render().unwrap();
        assert!(frame.contains("cancel this before any response"));
        assert!(frame.contains("response cancelled"));
        assert!(harness.state.chat[0]
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("cancelled before")));
    }

    #[test]
    fn visual_search_extends_the_fixed_anchor_and_chat_visual_yanks_messages() {
        let mut review = TuiHarness::from_unified_diff("search", workflow_diff(), 100, 24).unwrap();
        review.key(key(KeyCode::Char('V'))).unwrap();
        review.key(key(KeyCode::Char('/'))).unwrap();
        type_text(&mut review, "use_added");
        review.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(review.mode(), "VISUAL");
        assert!(review.render().unwrap().contains("rows 1-3"));

        let mut chat = TuiHarness::from_unified_diff("chat", workflow_diff(), 100, 24).unwrap();
        chat.key(key(KeyCode::Tab)).unwrap();
        chat.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut chat, "hello");
        chat.key(key(KeyCode::Enter)).unwrap();
        chat.stream_next_response(&["streamed ", "answer"]).unwrap();
        chat.render().unwrap();
        chat.key(key(KeyCode::Char('g'))).unwrap();
        chat.key(key(KeyCode::Char('g'))).unwrap();
        chat.key(key(KeyCode::Char('v'))).unwrap();
        chat.key(key(KeyCode::Char('G'))).unwrap();
        assert!(chat.render().unwrap().contains("VISUAL CHAR"));
        chat.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(
            chat.last_yank(),
            Some("you: hello\n\ncopilot: streamed answer")
        );
        assert!(chat.render().unwrap().contains("Yanked 36 bytes"));
    }

    #[test]
    fn chat_yy_yanks_the_whole_current_message_without_visual_mode() {
        let mut chat = TuiHarness::from_unified_diff("chat-yy", workflow_diff(), 100, 24).unwrap();
        chat.key(key(KeyCode::Tab)).unwrap();
        chat.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut chat, "whole current message");
        chat.key(key(KeyCode::Enter)).unwrap();
        chat.render().unwrap();
        chat.key(key(KeyCode::Char('g'))).unwrap();
        chat.key(key(KeyCode::Char('g'))).unwrap();
        chat.key(key(KeyCode::Char('y'))).unwrap();
        chat.key(key(KeyCode::Char('y'))).unwrap();

        assert_eq!(chat.last_yank(), Some("whole current message"));
        assert!(chat.render().unwrap().contains("Yanked 21 bytes"));
    }

    #[test]
    fn chat_character_visual_at_latest_selects_the_final_source_character() {
        let mut chat =
            TuiHarness::from_unified_diff("latest-character", workflow_diff(), 100, 24).unwrap();
        chat.key(key(KeyCode::Tab)).unwrap();
        chat.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut chat, "prompt");
        chat.key(key(KeyCode::Enter)).unwrap();
        chat.stream_next_response(&["Short answer to copy"])
            .unwrap();
        chat.render().unwrap();
        chat.key(key(KeyCode::Char('G'))).unwrap();
        chat.key(key(KeyCode::Char('v'))).unwrap();
        assert!(chat.selected_cell_count().unwrap() >= 1);
        chat.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(chat.last_yank(), Some("y"));
    }

    #[test]
    fn minimum_chat_latest_row_contains_source_text_instead_of_a_trailing_spacer() {
        let mut chat =
            TuiHarness::from_unified_diff("minimum-latest", workflow_diff(), 40, 9).unwrap();
        chat.key(key(KeyCode::Tab)).unwrap();
        chat.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut chat, "prompt");
        chat.key(key(KeyCode::Enter)).unwrap();
        chat.stream_next_response(&["latest visible answer"])
            .unwrap();
        chat.key(key(KeyCode::Char('G'))).unwrap();
        let frame = chat.render().unwrap();
        assert!(frame.contains("answer"), "{frame}");
    }

    #[test]
    fn chat_page_keys_extend_semantic_selection_across_wrap_resize_and_streaming() {
        let mut harness =
            TuiHarness::from_unified_diff("semantic-pages", workflow_diff(), 42, 14).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(
            &mut harness,
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau",
        );
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.render().unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::PageDown)).unwrap();
        assert_eq!(harness.mode(), "VISUAL");
        assert!(harness.selected_cell_count().unwrap() > 8);

        harness.resize(68, 18);
        assert!(harness.selected_cell_count().unwrap() > 8);
        harness
            .stream_next_response(&["streamed endpoint must not move the selection"])
            .unwrap();
        harness.key(key(KeyCode::Char('y'))).unwrap();
        assert!(harness
            .last_yank()
            .is_some_and(|text| text.starts_with("alpha beta gamma")));
    }

    #[test]
    fn chat_semantic_modes_map_markdown_code_and_unicode_without_message_wide_highlighting() {
        let mut harness =
            TuiHarness::from_unified_diff("semantic-modes", workflow_diff(), 52, 18).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(
            &mut harness,
            "## café 👩\u{200d}💻\n```rust\nfn main() { println!(\"漢字\"); }\n```",
        );
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.render().unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();

        harness.key(key(KeyCode::Char('V'))).unwrap();
        assert!(harness.render().unwrap().contains("VISUAL LINE"));
        assert!(harness.selected_cell_count().unwrap() > 2);
        harness.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(harness.last_yank(), Some("café 👩\u{200d}💻\n"));

        harness
            .key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        let frame = harness.render().unwrap();
        assert!(frame.contains("VISUAL BLOCK"));
        assert!(frame.contains("fn main()"));
        assert!(harness.selected_cell_count().unwrap() > 1);
    }

    #[test]
    fn chat_copy_omits_markdown_decoration_cells() {
        let mut harness =
            TuiHarness::from_unified_diff("markdown-copy", workflow_diff(), 52, 18).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness
            .inject_agent_event(AgentEvent::HistoryLoaded(vec![HistoryEntry {
                role: "assistant".into(),
                text: "# Heading\n- item".into(),
            }]))
            .unwrap();
        harness.render().unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('V'))).unwrap();
        harness.key(key(KeyCode::Char('G'))).unwrap();
        harness.key(key(KeyCode::Char('y'))).unwrap();

        assert_eq!(harness.last_yank(), Some("Heading\nitem\n"));
    }

    #[test]
    fn annotation_edit_delete_undo_and_inline_follow_up_are_complete_workflows() {
        let mut comment = TuiHarness::from_unified_diff("edit", workflow_diff(), 100, 24).unwrap();
        comment.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut comment, "original");
        comment.key(key(KeyCode::Enter)).unwrap();
        comment.key(key(KeyCode::Char('e'))).unwrap();
        type_text(&mut comment, " updated");
        comment.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(
            comment.persisted_annotations().unwrap()[0].text.as_deref(),
            Some("original updated")
        );
        comment.key(key(KeyCode::Char('d'))).unwrap();
        comment.key(key(KeyCode::Char('d'))).unwrap();
        assert!(comment.persisted_annotations().unwrap().is_empty());
        assert!(comment.status().contains("u undo"));
        comment.key(key(KeyCode::Char('u'))).unwrap();
        assert_eq!(comment.persisted_annotations().unwrap().len(), 1);

        let mut ask = TuiHarness::from_unified_diff("follow-up", workflow_diff(), 100, 24).unwrap();
        ask.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut ask, "first question");
        ask.key(key(KeyCode::Enter)).unwrap();
        ask.stream_next_response(&["first ", "answer"]).unwrap();
        let completed_stream = ask.state.review_stream();
        let completed_rows: Vec<_> = completed_stream
            .rows()
            .iter()
            .filter_map(|row| match row {
                crate::review_stream::ReviewRow::Annotation { block, .. }
                    if block.annotation_id != crate::app::INLINE_COMPOSER_ID =>
                {
                    Some(block)
                }
                _ => None,
            })
            .collect();
        assert!(completed_rows.iter().any(|row| {
            matches!(row.part, crate::review_stream::AnnotationRowPart::Prompt)
                && row.text.contains("follow up")
        }));
        assert!(completed_rows
            .iter()
            .all(|row| row.annotation_id == completed_rows[0].annotation_id));
        ask.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(ask.mode(), "INSERT");
        type_text(&mut ask, "follow-up question");
        let active_stream = ask.state.review_stream();
        let active_rows: Vec<_> = active_stream
            .rows()
            .iter()
            .filter_map(|row| match row {
                crate::review_stream::ReviewRow::Annotation { block, .. }
                    if block.annotation_id != crate::app::INLINE_COMPOSER_ID =>
                {
                    Some(block)
                }
                _ => None,
            })
            .collect();
        assert!(active_rows.iter().any(|row| {
            matches!(row.part, crate::review_stream::AnnotationRowPart::Prompt)
                && row.text.contains("follow-up question")
                && row.text.contains('▏')
        }));
        assert!(active_rows
            .iter()
            .all(|row| row.annotation_id == active_rows[0].annotation_id));
        let prompt_before = active_stream
            .rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    crate::review_stream::ReviewRow::Annotation { block, .. }
                        if block.annotation_id == active_rows[0].annotation_id
                            && matches!(block.part, crate::review_stream::AnnotationRowPart::Prompt)
                )
            })
            .unwrap();
        let annotation_id = active_rows[0].annotation_id.clone();
        let thread = ask.state.ask_threads.get_mut(&annotation_id).unwrap();
        let mut streamed = thread.last().cloned().unwrap();
        streamed.id = "synthetic-streamed-follow-up".into();
        streamed.seq += 1;
        streamed.role = "assistant".into();
        streamed.text = "a response inserted above the still-editable prompt".into();
        streamed.sent = true;
        streamed.delivery_state = DeliveryState::Sent;
        streamed.ts = "2026-01-01T00:00:00Z".into();
        thread.push(streamed);
        ask.render().unwrap();
        assert!(ask.state.review_cursor > prompt_before);
        let active_frame = ask.render().unwrap();
        assert!(active_frame.contains("follow-up question"));
        assert!(active_frame.contains('▏'));
        assert!(!active_frame.contains("Ask follow-up · draft"));
        ask.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(ask.agent_commands().len(), 2);
        ask.stream_next_response(&["follow-up ", "answer"]).unwrap();
        ask.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut ask, "diff unified");
        ask.key(key(KeyCode::Enter)).unwrap();
        let frame = ask.render().unwrap();
        assert!(frame.contains("follow-up question"));
        assert!(frame.contains("follow-up answer"));
    }

    #[test]
    fn ask_follow_up_affordance_explains_side_mode_is_disabled() {
        let mut harness =
            TuiHarness::from_unified_diff("side-follow-up-affordance", workflow_diff(), 100, 24)
                .unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "question");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.state.side_active = true;
        let frame = harness.render().unwrap();
        assert!(frame.contains("follow-up unavailable · /main"));
    }

    #[test]
    fn comment_export_queues_one_batch_and_acknowledges_delivery() {
        let mut harness =
            TuiHarness::from_unified_diff("export", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "export me");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "export");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(harness.agent_commands().len(), 1);
        assert!(harness.status().contains("Comment batch queued"));
        assert_eq!(harness.state.screen, Screen::Chat);
        assert_eq!(harness.state.focus, Focus::Chat);
        assert!(harness.render().unwrap().contains("comments submitted"));
        assert_eq!(
            harness.persisted_annotations().unwrap()[0].delivery_state,
            "pending"
        );
        harness
            .stream_next_response(&["Batch ", "received"])
            .unwrap();
        let annotation = harness.persisted_annotations().unwrap().remove(0);
        assert!(annotation.submitted);
        assert_eq!(annotation.delivery_state, "sent");
    }

    #[test]
    fn delivered_annotation_correction_requires_explicit_recovery_after_restart() {
        let mut harness =
            TuiHarness::from_unified_diff("correction-recovery", workflow_diff(), 90, 22).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "original comment");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "export");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.stream_next_response(&["batch received"]).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char(']'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        harness.key(key(KeyCode::Char('e'))).unwrap();
        harness.state.compose.clear();
        harness.state.compose_cursor = 0;
        type_text(&mut harness, "revised comment");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert!(harness.status().contains("correction queued"));
        assert_eq!(
            harness
                .storage
                .pending_chats(&harness.state.work_item.item.id)
                .unwrap()[0]
                .kind,
            "correction"
        );

        harness.restart().unwrap();
        assert_eq!(harness.state.screen, Screen::Recovery);
        assert!(harness.render().unwrap().contains("MAIN correction"));
        harness.key(key(KeyCode::Char('r'))).unwrap();
        assert!(harness.status().contains("correction intentionally resent"));
        harness
            .stream_next_response(&["correction received"])
            .unwrap();
        harness.restart().unwrap();

        assert_ne!(harness.state.screen, Screen::Recovery);
        assert!(harness
            .storage
            .pending_chats(&harness.state.work_item.item.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn comment_export_in_side_uses_the_side_lane_and_transcript() {
        let mut harness =
            TuiHarness::from_unified_diff("side-export", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut harness, "export only to active side");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "export");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.starts_with("SendSide(") && command.contains("CommentBatch")));
        assert!(harness
            .state
            .chat
            .iter()
            .any(|entry| entry.role == "comments"));
        assert!(harness
            .state
            .main_chat
            .as_ref()
            .is_some_and(|chat| chat.iter().all(|entry| entry.role != "comments")));
    }

    #[test]
    fn binary_empty_and_renamed_files_have_explicit_safe_behavior() {
        let binary = "diff --git a/image.png b/image.png\n\
                      new file mode 100644\n\
                      Binary files /dev/null and b/image.png differ\n";
        let mut binary = TuiHarness::from_unified_diff("binary", binary, 90, 20).unwrap();
        binary.key(key(KeyCode::Char('c'))).unwrap();
        assert_eq!(binary.mode(), "NORMAL");
        assert!(binary.status().contains("binary or empty"));

        let renamed = concat!(
            "diff --git a/old.rs b/new.rs\n",
            "similarity index 80%\n",
            "rename from old.rs\n",
            "rename to new.rs\n",
            "@@ -1 +1 @@\n",
            "-old();\n",
            "+new();\n",
        );
        let mut renamed = TuiHarness::from_unified_diff("renamed", renamed, 90, 20).unwrap();
        renamed.key(key(KeyCode::Char('j'))).unwrap();
        renamed.key(key(KeyCode::Char('c'))).unwrap();
        type_text(&mut renamed, "renamed anchor");
        renamed.key(key(KeyCode::Enter)).unwrap();
        let annotation = renamed.persisted_annotations().unwrap().remove(0);
        assert_eq!(annotation.file, "new.rs");
        assert_eq!(annotation.side, "new");
        assert_eq!((annotation.line_start, annotation.line_end), (1, 1));
    }

    #[test]
    fn page_movement_extends_visual_anchor_and_chat_cancel_discards_draft() {
        let mut large = String::from(
            "diff --git a/large.rs b/large.rs\n--- a/large.rs\n+++ b/large.rs\n@@ -0,0 +1,40 @@\n",
        );
        for index in 1..=40 {
            large.push_str(&format!("+line {index}\n"));
        }
        let mut review = TuiHarness::from_unified_diff("large", &large, 90, 16).unwrap();
        review.render().unwrap();
        review.key(key(KeyCode::Char('V'))).unwrap();
        review.key(key(KeyCode::PageDown)).unwrap();
        assert_eq!(review.mode(), "VISUAL");
        assert!(review.render().unwrap().contains("VISUAL LINE · rows 1-"));
        assert!(review.selected_cell_count().unwrap() > 0);

        let mut chat =
            TuiHarness::from_unified_diff("chat-cancel", workflow_diff(), 90, 20).unwrap();
        chat.key(key(KeyCode::Tab)).unwrap();
        chat.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut chat, "discard chat draft");
        chat.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(chat.mode(), "NORMAL");
        assert!(chat.render().unwrap().contains("discard chat draft"));
        assert!(chat.status().contains("Draft kept"));
        chat.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(!chat.render().unwrap().contains("discard chat draft"));
        assert!(chat.agent_commands().is_empty());
    }

    #[test]
    fn closed_agent_channel_keeps_one_pending_ask_without_duplicate_retry() {
        let mut harness =
            TuiHarness::from_unified_diff("closed-agent", workflow_diff(), 90, 20).unwrap();
        harness.set_agent_send_failure(true);
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "persist once");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert!(harness.status().contains("saved as pending"));
        assert_eq!(harness.persisted_annotations().unwrap().len(), 1);
        assert_eq!(harness.pending_outbound_count(), 0);

        harness.restart().unwrap();
        assert!(harness.render().unwrap().contains("Delivery recovery"));
        assert_eq!(harness.persisted_annotations().unwrap().len(), 1);
    }

    #[test]
    fn command_palette_is_scrollable_selectable_and_keeps_the_sticky_composer() {
        let mut harness =
            TuiHarness::from_unified_diff("commands", workflow_diff(), 92, 22).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "preserved draft");
        harness.key(key(KeyCode::Esc)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        let initial = harness.render().unwrap();
        assert!(initial.contains("COMMAND COMPLETIONS"));
        assert!(initial.contains("COMMAND MODE ACTIVE"));
        assert!(initial.contains("draft 15 bytes held"));
        assert!(initial.contains("↑/↓ wrap"));
        assert!(initial.contains(":█"));
        assert!(!initial.contains("Type a message"));
        assert!(!initial.contains("preserved draft"));

        for _ in 0..12 {
            harness.key(key(KeyCode::Down)).unwrap();
        }
        let scrolled = harness.render().unwrap();
        assert!(scrolled.contains("▶ :"));
        harness.key(key(KeyCode::Tab)).unwrap();
        assert!(harness.render().unwrap().contains(":"));
        harness.key(key(KeyCode::Esc)).unwrap();
        assert!(harness.render().unwrap().contains("preserved draft"));

        let mut compact =
            TuiHarness::from_unified_diff("compact commands", workflow_diff(), 42, 9).unwrap();
        compact.key(key(KeyCode::Tab)).unwrap();
        compact.key(key(KeyCode::Char(':'))).unwrap();
        compact.render().unwrap();
        for _ in 0..7 {
            compact.key(key(KeyCode::Down)).unwrap();
        }
        let compact_frame = compact.render().unwrap();
        assert!(
            compact_frame.contains("▶ :"),
            "selected command must remain visible:\n{compact_frame}"
        );
        assert!(compact_frame.contains("COPILOT"));

        let mut empty =
            TuiHarness::from_unified_diff("empty commands", workflow_diff(), 42, 12).unwrap();
        empty.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut empty, "not-a-real-command");
        let empty_frame = empty.render().unwrap();
        assert!(empty_frame.contains("No matching commands"));
        empty.key(key(KeyCode::Esc)).unwrap();
        let closed = empty.render().unwrap();
        assert!(closed.contains("Command palette closed"));
        assert!(!closed.contains("NORMAL · NORMAL"));

        let mut long =
            TuiHarness::from_unified_diff("long command", workflow_diff(), 42, 9).unwrap();
        long.key(key(KeyCode::Tab)).unwrap();
        long.key(key(KeyCode::Char(':'))).unwrap();
        type_text(
            &mut long,
            "steer this correction must keep its trailing cursor visible",
        );
        let long_frame = long.render().unwrap();
        assert!(long_frame.contains('…'));
        assert!(long_frame.contains("trailing cursor visible█"));

        let mut escape =
            TuiHarness::from_unified_diff("command escape", workflow_diff(), 80, 12).unwrap();
        escape.key(key(KeyCode::Tab)).unwrap();
        escape.key(key(KeyCode::Char(':'))).unwrap();
        escape.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(escape.mode(), "NORMAL");
        assert!(!escape.status().contains("COMMAND mode"));
        assert!(escape.status().contains("Command palette closed"));
    }

    #[test]
    fn subsequent_questions_enter_a_visible_background_fifo_queue() {
        let mut harness = TuiHarness::from_unified_diff("queue", workflow_diff(), 92, 22).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        for prompt in ["first background question", "second queued follow-up"] {
            harness.key(key(KeyCode::Char('i'))).unwrap();
            type_text(&mut harness, prompt);
            harness.key(key(KeyCode::Enter)).unwrap();
        }
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "queue");
        harness.key(key(KeyCode::Enter)).unwrap();
        let queue = harness.render().unwrap();
        assert!(queue.contains("Copilot queue · 2 pending · background FIFO"));
        assert!(queue.contains("▶ #1"));
        assert!(queue.contains("  #2"));
        assert!(queue.contains("first background question"));
        assert!(queue.contains("second queued follow-up"));
        assert!(!queue.contains("ACTIVE"));
        assert!(queue.contains("d cancel"));

        let second_id = harness.state.queue_entry_ids()[1].0.clone();
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Char('d'))).unwrap();
        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.contains("CancelQueued") && command.contains(&second_id)));
        let queue = harness.render().unwrap();
        assert!(queue.contains("1 pending"));
        assert!(!queue.contains("second queued follow-up"));

        let mut narrow =
            TuiHarness::from_unified_diff("narrow queue", workflow_diff(), 40, 9).unwrap();
        narrow.key(key(KeyCode::Tab)).unwrap();
        narrow.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut narrow, "queued prompt");
        narrow.key(key(KeyCode::Enter)).unwrap();
        narrow.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut narrow, "queue");
        narrow.key(key(KeyCode::Enter)).unwrap();
        let narrow_queue = narrow.render().unwrap();
        assert!(narrow_queue.contains("e edit"));
        assert!(narrow_queue.contains("d cancel"));
        assert!(narrow_queue.contains("s stop"));
        assert!(narrow_queue.contains("· q"));
    }

    #[test]
    fn queue_selection_ignores_the_active_assistant_stream_entry() {
        let mut harness =
            TuiHarness::from_unified_diff("active queue", workflow_diff(), 72, 20).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "first prompt");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.start_next_response("partial answer").unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "second prompt");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "queue");
        harness.key(key(KeyCode::Enter)).unwrap();

        let queued_id = harness.state.queue_entry_ids()[1].0.clone();
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Char('d'))).unwrap();
        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.contains("CancelQueued") && command.contains(&queued_id)));
    }

    #[test]
    fn steering_is_immediate_and_visible_without_blocking_the_composer() {
        let mut harness = TuiHarness::from_unified_diff("steer", workflow_diff(), 92, 22).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.state.agent_progress.phase = AgentPhase::Responding;
        harness.state.agent_progress.active_outbound_id = Some("active-turn".into());
        harness
            .inject_agent_event(AgentEvent::Activity {
                outbound_id: None,
                label: "background hook activity".into(),
            })
            .unwrap();
        assert_eq!(
            harness.state.agent_progress.active_outbound_id.as_deref(),
            Some("active-turn")
        );
        harness
            .inject_activity(
                ActivityKind::Intent,
                "typed background activity",
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            harness.state.agent_progress.active_outbound_id.as_deref(),
            Some("active-turn")
        );
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/steer inspect the cleanup branch");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.contains("Steer") && command.contains("cleanup branch")));
        let frame = harness.render().unwrap();
        assert!(frame.contains("you · steer"));
        assert!(frame.contains("inspect the cleanup branch"));
        assert!(harness.status().contains("Steering accepted immediately"));
        assert_eq!(harness.pending_outbound_count(), 0);
        assert!(harness
            .storage
            .pending_chats(&harness.state.work_item.item.id)
            .unwrap()
            .is_empty());

        harness.restart().unwrap();
        assert_ne!(harness.state.screen, crate::app::Screen::Recovery);
        assert!(harness.state.pending_chats.is_empty());
    }

    #[test]
    fn model_picker_drills_into_runtime_reasoning_and_context_capabilities() {
        let mut harness =
            TuiHarness::from_unified_diff("models", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "model");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command == "ListModels"));
        harness
            .inject_agent_event(AgentEvent::ModelsListed(vec![ModelOption {
                id: "capable-model".into(),
                name: "Capable Model".into(),
                supported_reasoning_efforts: vec!["low".into(), "high".into()],
                default_reasoning_effort: Some("low".into()),
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
            }]))
            .unwrap();
        assert!(harness
            .render()
            .unwrap()
            .contains("Model picker · step 1/3"));
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness
            .render()
            .unwrap()
            .contains("step 2/3 · Choose reasoning effort"));
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness
            .render()
            .unwrap()
            .contains("step 3/3 · Choose context tier"));
        harness.key(key(KeyCode::Esc)).unwrap();
        assert!(harness
            .render()
            .unwrap()
            .contains("step 2/3 · Choose reasoning effort"));
        harness.key(key(KeyCode::Esc)).unwrap();
        assert!(harness
            .render()
            .unwrap()
            .contains("Model picker · step 1/3"));
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Down)).unwrap();
        harness.key(key(KeyCode::Enter)).unwrap();
        let commands = harness.agent_commands();
        let selection = commands
            .iter()
            .find(|command| command.starts_with("SelectModel"))
            .expect("staged selection command");
        assert!(selection.contains("capable-model"));
        assert!(selection.contains("high"));
        assert!(selection.contains("long_context"));
    }

    #[test]
    fn model_picker_is_explicitly_main_scoped_while_side_is_active() {
        let mut harness =
            TuiHarness::from_unified_diff("side-model", workflow_diff(), 90, 20).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated model context");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "model");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert_ne!(harness.state.screen, crate::app::Screen::ModelPicker);
        assert!(harness.status().contains("MAIN-scoped"));
        assert!(!harness
            .agent_commands()
            .iter()
            .any(|command| command == "ListModels"));
    }

    #[test]
    fn model_picker_skips_capability_stages_the_runtime_does_not_offer() {
        let mut harness =
            TuiHarness::from_unified_diff("fixed-model", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "model");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_agent_event(AgentEvent::ModelsListed(vec![ModelOption {
                id: "fixed-model".into(),
                name: "Fixed Model".into(),
                supported_reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                max_context_tokens: Some(64_000),
                context_tiers: Vec::new(),
            }]))
            .unwrap();
        harness.key(key(KeyCode::Enter)).unwrap();

        assert_ne!(harness.state.screen, crate::app::Screen::ModelPicker);
        let selection = harness
            .agent_commands()
            .into_iter()
            .find(|command| command.starts_with("SelectModel"))
            .expect("fixed-capability model is applied immediately");
        assert!(selection.contains("fixed-model"));
        assert!(selection.contains("reasoning_effort: None"));
        assert!(selection.contains("context_tier: None"));
    }

    #[test]
    fn model_picker_numbers_only_runtime_supported_stages() {
        let mut context_only =
            TuiHarness::from_unified_diff("context-only", workflow_diff(), 100, 24).unwrap();
        context_only.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut context_only, "model");
        context_only.key(key(KeyCode::Enter)).unwrap();
        context_only
            .inject_agent_event(AgentEvent::ModelsListed(vec![ModelOption {
                id: "context-model".into(),
                name: "Context Model".into(),
                supported_reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                max_context_tokens: Some(128_000),
                context_tiers: vec![ContextTierOption {
                    id: "long_context".into(),
                    max_context_tokens: Some(256_000),
                }],
            }]))
            .unwrap();
        assert!(context_only.render().unwrap().contains("step 1/2"));
        context_only.key(key(KeyCode::Enter)).unwrap();
        assert!(context_only
            .render()
            .unwrap()
            .contains("step 2/2 · Choose context tier"));

        let mut reasoning_only =
            TuiHarness::from_unified_diff("reasoning-only", workflow_diff(), 100, 24).unwrap();
        reasoning_only.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut reasoning_only, "model");
        reasoning_only.key(key(KeyCode::Enter)).unwrap();
        reasoning_only
            .inject_agent_event(AgentEvent::ModelsListed(vec![ModelOption {
                id: "reasoning-model".into(),
                name: "Reasoning Model".into(),
                supported_reasoning_efforts: vec!["low".into(), "high".into()],
                default_reasoning_effort: Some("low".into()),
                max_context_tokens: Some(128_000),
                context_tiers: Vec::new(),
            }]))
            .unwrap();
        assert!(reasoning_only.render().unwrap().contains("step 1/2"));
        reasoning_only.key(key(KeyCode::Enter)).unwrap();
        assert!(reasoning_only
            .render()
            .unwrap()
            .contains("step 2/2 · Choose reasoning effort"));
    }

    #[test]
    fn minimum_model_picker_keeps_confirmation_and_cancel_controls_visible() {
        let mut harness =
            TuiHarness::from_unified_diff("minimum-model", workflow_diff(), 40, 9).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "model");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_agent_event(AgentEvent::ModelsListed(vec![ModelOption {
                id: "compact-model".into(),
                name: "Compact Model".into(),
                supported_reasoning_efforts: vec!["high".into()],
                default_reasoning_effort: Some("high".into()),
                max_context_tokens: Some(128_000),
                context_tiers: vec![ContextTierOption {
                    id: "default".into(),
                    max_context_tokens: Some(128_000),
                }],
            }]))
            .unwrap();

        let frame = harness.render().unwrap();
        assert!(frame.contains("Compact Model"));
        assert!(frame.contains("compact-model"));
        assert!(frame.contains("↑/↓ select"));
        assert!(frame.contains("Enter next"));
        assert!(frame.contains("Esc cancel"));
    }

    #[test]
    fn review_ask_composer_expands_wraps_and_scrolls_without_overflow() {
        let mut harness =
            TuiHarness::from_unified_diff("ask-composer", workflow_diff(), 48, 18).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(
            &mut harness,
            concat!(
                "How does this work here? How can we make it better? What do we need to do ",
                "to make it better? Why did this overflow from the old fixed-height box so ",
                "easily? The replacement computes wrapped rows, expands to a safe height, ",
                "and keeps the cursor visible through an internal vertical viewport."
            ),
        );
        let frame = harness.render().unwrap();
        assert!(frame.contains("Ask"));
        assert!(frame.contains("lines "));
        assert!(frame.contains("↑/↓ scroll"));
        assert!(frame.contains("cursor visible"));
        assert!(frame.contains("vertical viewport."));
        assert!(frame.contains('▏'));
        assert!(frame.contains("Enter submit"));
    }

    #[test]
    fn sticky_chat_composer_wraps_edits_preserves_and_explicitly_discards_drafts() {
        let mut harness =
            TuiHarness::from_unified_diff("composer", workflow_diff(), 48, 16).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(
            &mut harness,
            "a long prompt that must wrap across several rows in the sticky editor",
        );
        harness
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .unwrap();
        type_text(&mut harness, "second line");
        harness.key(key(KeyCode::Left)).unwrap();
        harness.key(key(KeyCode::Char('!'))).unwrap();
        let composing = harness.render().unwrap();
        assert!(composing.contains("INSERT · Enter send"));
        assert!(composing.contains("long prompt"));
        assert!(composing.contains("second lin!e"));

        harness.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert!(harness.render().unwrap().contains("second lin!e"));
        harness
            .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(harness.compose_text().is_empty());
        assert!(harness.status().contains("Draft cancelled"));
    }

    #[test]
    fn chat_scrolls_by_rendered_rows_and_pauses_live_following() {
        let mut harness =
            TuiHarness::from_unified_diff("row-scroll", workflow_diff(), 44, 14).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(
            &mut harness,
            concat!(
                "one message with enough words to occupy many wrapped terminal rows and keep ",
                "going through several paragraphs worth of material so scrolling is mandatory ",
                "inside this single visual transcript entry instead of skipping whole messages"
            ),
        );
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.render().unwrap();
        let live_bottom = harness.chat_scroll();
        let viewport = harness.state.chat_viewport_rows.max(1);
        harness.key(key(KeyCode::Up)).unwrap();
        assert_eq!(harness.chat_scroll(), live_bottom.saturating_sub(1));
        let before_page = harness.chat_scroll();
        harness.key(key(KeyCode::PageUp)).unwrap();
        assert_eq!(harness.chat_scroll(), before_page.saturating_sub(viewport));
        let before_half_page = harness.chat_scroll();
        harness
            .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(
            harness.chat_scroll(),
            before_half_page.saturating_add((viewport / 2).max(1))
        );
        let paused = harness.render().unwrap();
        assert!(paused.contains("rows "));
        harness.key(key(KeyCode::Char('G'))).unwrap();
        harness.render().unwrap();
        assert!(harness.chat_scroll() >= live_bottom);
    }

    #[test]
    fn side_restores_main_chat_viewport_navigation_and_selection_exactly() {
        let mut harness =
            TuiHarness::from_unified_diff("side-viewport", workflow_diff(), 48, 14).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(
            &mut harness,
            "MAIN transcript text long enough to build a real scrollable semantic layout",
        );
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.render().unwrap();

        let navigation = harness
            .state
            .chat_navigation
            .clone()
            .expect("render creates a semantic chat cursor");
        harness.state.chat_selection = Some(crate::chat_selection::ChatSelection::character(
            navigation.point.clone(),
        ));
        harness.state.chat_cursor = 0;
        harness.state.chat_scroll = 1;
        harness.state.chat_autofollow = false;
        let expected = (
            harness.state.focus,
            harness.state.input_mode,
            harness.state.chat_cursor,
            harness.state.chat_scroll,
            harness.state.chat_total_rows,
            harness.state.chat_viewport_rows,
            harness.state.chat_autofollow,
            harness.state.chat_navigation.clone(),
            harness.state.chat_selection.clone(),
        );

        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated viewport");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        assert!(harness.state.chat_selection.is_none());

        harness
            .inject_side_exited("main-session", "side-session")
            .unwrap();
        let restored = (
            harness.state.focus,
            harness.state.input_mode,
            harness.state.chat_cursor,
            harness.state.chat_scroll,
            harness.state.chat_total_rows,
            harness.state.chat_viewport_rows,
            harness.state.chat_autofollow,
            harness.state.chat_navigation.clone(),
            harness.state.chat_selection.clone(),
        );
        assert_eq!(restored, expected);
        assert!(harness.state.main_chat.is_none());
        assert!(harness.state.pending_side_entries.is_empty());
    }

    #[test]
    fn side_active_cannot_create_an_orphaned_main_ask_in_the_side_transcript() {
        let mut harness =
            TuiHarness::from_unified_diff("side-main-ask", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();

        harness.key(key(KeyCode::Char('g'))).unwrap();
        harness.key(key(KeyCode::Char('r'))).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();

        assert_eq!(harness.mode(), "NORMAL");
        assert!(harness
            .status()
            .contains("MAIN Ask is unavailable while SIDE is active"));
        assert!(harness
            .state
            .chat
            .iter()
            .all(|entry| entry.annotation_id.is_none()));
        assert!(harness.state.pending_side_entries.is_empty());
    }

    #[test]
    fn liveness_panel_exposes_quiet_sdk_diagnostics_and_tool_skill_history() {
        let mut harness =
            TuiHarness::from_unified_diff("liveness", workflow_diff(), 110, 26).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "inspect the change");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_activity(
                ActivityKind::ToolStart,
                "Running security-review skill",
                Some("skill".into()),
                Some("security-review".into()),
            )
            .unwrap();
        harness.backdate_agent_progress(Duration::from_secs(20));
        let quiet = harness.render().unwrap();
        assert!(quiet.contains("no SDK events for"));
        assert!(quiet.contains(":agent-status"));
        harness.state.agent_connected = true;
        harness.resize(40, 9);
        let compact_quiet = harness.render().unwrap();
        assert!(compact_quiet.contains("QUIET"));
        harness.resize(110, 26);
        for index in 0..12 {
            harness
                .inject_activity(
                    ActivityKind::ToolProgress,
                    format!("background audit step {index}"),
                    Some("audit".into()),
                    None,
                )
                .unwrap();
        }

        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "agent-status");
        harness.key(key(KeyCode::Enter)).unwrap();
        let diagnostics = harness.render().unwrap();
        assert!(diagnostics.contains("COPILOT SDK LIVENESS"));
        assert!(diagnostics.contains("Running security-review skill"));
        assert!(diagnostics.contains("last SDK event"));
        assert!(diagnostics.contains("outbound id"));
        harness.key(key(KeyCode::End)).unwrap();
        let end = harness.state.scroll;
        assert_eq!(
            end,
            harness
                .state
                .agent_progress
                .timeline
                .len()
                .saturating_sub(1)
        );
        harness.key(key(KeyCode::PageUp)).unwrap();
        assert!(harness.state.scroll < end);
        let before_wheel = harness.state.scroll;
        harness.mouse_scroll(false).unwrap();
        assert!(harness.state.scroll > before_wheel);
        harness.key(key(KeyCode::Home)).unwrap();
        assert_eq!(harness.state.scroll, 0);

        harness.resize(40, 9);
        let narrow_diagnostics = harness.render().unwrap();
        assert!(narrow_diagnostics.contains("j/k scroll"));
        assert!(narrow_diagnostics.contains("s/C-c stop"));
        assert!(narrow_diagnostics.contains("q/Esc back"));
        assert!(narrow_diagnostics.contains("Recent SDK activity"));
        assert!(narrow_diagnostics.contains('/'));
        let mut activity_visible = narrow_diagnostics.contains("Running security");
        for _ in 0..16 {
            if activity_visible {
                break;
            }
            harness.key(key(KeyCode::Char('j'))).unwrap();
            activity_visible = harness.render().unwrap().contains("Running security");
        }
        assert!(
            activity_visible,
            "compact timeline activity was unreachable"
        );

        let mut standard =
            TuiHarness::from_unified_diff("standard-liveness", workflow_diff(), 72, 18).unwrap();
        standard.key(key(KeyCode::Tab)).unwrap();
        standard
            .inject_activity(
                ActivityKind::Other,
                "running review skill: exhaustive audit",
                None,
                Some("ui-script injection".into()),
            )
            .unwrap();
        let standard_frame = standard.render().unwrap();
        assert!(standard_frame.contains("running review skill: exhaustive audit"));
        assert!(standard_frame.contains("· q0"));
        standard
            .inject_activity(
                ActivityKind::Failure,
                "Subagent edge auditor failed",
                Some("subagent:edge-auditor".into()),
                Some("controlled failure details".into()),
            )
            .unwrap();
        let failure_frame = standard.render().unwrap();
        assert_eq!(standard.state.agent_progress.phase, AgentPhase::Failed);
        assert!(failure_frame.contains("FAILED"));
        assert!(failure_frame.contains("Subagent edge auditor failed"));
        standard.resize(40, 9);
        let compact_failure = standard.render().unwrap();
        assert!(
            compact_failure.contains("FAILED"),
            "compact failure phase was hidden:\n{compact_failure}"
        );
        assert!(
            compact_failure.contains("Subagent edge auditor"),
            "compact failure summary was hidden:\n{compact_failure}"
        );
    }

    #[test]
    fn disconnect_preserves_the_age_of_the_last_healthy_sdk_event() {
        let mut harness =
            TuiHarness::from_unified_diff("disconnect-age", workflow_diff(), 100, 20).unwrap();
        harness.state.agent_connected = true;
        harness.backdate_agent_progress(Duration::from_secs(20));
        harness
            .inject_agent_event(AgentEvent::Error("transport closed".into()))
            .unwrap();

        let frame = harness.render().unwrap();
        assert!(frame.contains("OFFLINE"));
        assert!(frame.contains("last SDK event 20s"));
        assert!(!frame.contains("last SDK event 0ms"));
    }

    #[test]
    fn side_conversation_is_visibly_isolated_and_main_transcript_is_restored() {
        let mut harness = TuiHarness::from_unified_diff("side", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "persistent main message");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.stream_next_response(&["main answer"]).unwrap();

        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side ephemeral secret");
        harness.key(key(KeyCode::Enter)).unwrap();
        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.contains("StartSide")));
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        let side = harness.render().unwrap();
        assert!(side.contains("SIDE"));
        assert!(side.contains("ephemeral secret"));
        assert!(!side.contains("persistent main message"));
        harness.stream_next_response(&["side answer"]).unwrap();

        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/main");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_exited("main-session", "side-session")
            .unwrap();
        let main = harness.render().unwrap();
        assert!(main.contains("MAIN"));
        assert!(main.contains("persistent main message"));
        assert!(!main.contains("ephemeral secret"));
        assert!(!main.contains("side answer"));
    }

    #[test]
    fn side_round_trip_restores_the_main_draft_cursor_and_input_state() {
        let mut harness =
            TuiHarness::from_unified_diff("side-draft", workflow_diff(), 80, 20).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "MAIN preserved draft");
        harness.key(key(KeyCode::Esc)).unwrap();
        let main_cursor = harness.state.compose_cursor;
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "side");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();

        assert!(harness.state.compose.is_empty());
        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "main");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert_eq!(harness.state.compose, "MAIN preserved draft");
        assert_eq!(harness.state.compose_cursor, main_cursor);
        assert_eq!(harness.state.input_mode, crate::app::InputMode::Normal);
        assert_eq!(
            harness.state.compose_target,
            Some(crate::app::ComposeTarget::Chat)
        );
    }

    #[test]
    fn inline_ask_composer_intercepts_side_instead_of_creating_an_annotation() {
        let mut harness =
            TuiHarness::from_unified_diff("inline-side", workflow_diff(), 80, 20).unwrap();
        harness.key(key(KeyCode::Char('a'))).unwrap();
        type_text(&mut harness, "/side inspect this separately");
        harness.key(key(KeyCode::Enter)).unwrap();

        assert_eq!(harness.state.screen, Screen::Chat);
        assert!(harness
            .agent_commands()
            .iter()
            .any(|command| command.contains("StartSide")));
        assert!(harness.persisted_annotations().unwrap().is_empty());
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        let frame = harness.render().unwrap();
        assert!(frame.contains("SIDE"));
        assert!(frame.contains("inspect this separately"));
    }

    #[test]
    fn chat_search_finds_current_message_occurrences_in_both_directions() {
        let mut harness =
            TuiHarness::from_unified_diff("chat-search", workflow_diff(), 80, 20).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness
            .inject_agent_event(AgentEvent::HistoryLoaded(vec![HistoryEntry {
                role: "assistant".into(),
                text: "foo middle foo end".into(),
            }]))
            .unwrap();
        harness.render().unwrap();
        harness.key(key(KeyCode::Char('/'))).unwrap();
        type_text(&mut harness, "foo");
        harness.key(key(KeyCode::Enter)).unwrap();
        let first = harness
            .state
            .chat_navigation
            .as_ref()
            .unwrap()
            .point
            .byte_offset;
        harness.key(key(KeyCode::Char('n'))).unwrap();
        let second = harness
            .state
            .chat_navigation
            .as_ref()
            .unwrap()
            .point
            .byte_offset;
        harness.key(key(KeyCode::Char('N'))).unwrap();
        let previous = harness
            .state
            .chat_navigation
            .as_ref()
            .unwrap()
            .point
            .byte_offset;

        assert_eq!(first, 0);
        assert_eq!(second, 11);
        assert_eq!(previous, first);
    }

    #[test]
    fn bracketed_multiline_paste_is_inserted_without_submitting() {
        let mut harness = TuiHarness::from_unified_diff("paste", workflow_diff(), 80, 20).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        let effects = harness
            .state
            .handle_paste("first line\r\nsecond line\rthird line");

        assert!(effects.is_empty());
        assert_eq!(
            harness.compose_text(),
            "first line\nsecond line\nthird line"
        );
        assert_eq!(harness.mode(), "INSERT");
        assert!(harness.status().contains("across 3 lines"));
        assert!(!harness
            .captured_effects()
            .iter()
            .any(|effect| effect.contains("SendChat")));

        let mut compact =
            TuiHarness::from_unified_diff("compact paste", workflow_diff(), 40, 9).unwrap();
        compact.key(key(KeyCode::Tab)).unwrap();
        compact.key(key(KeyCode::Char('i'))).unwrap();
        compact
            .paste(
                &(1..=12)
                    .map(|line| format!("pasted line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
        let bottom = compact.render().unwrap();
        assert!(bottom.contains("PASTE 12-12/12"));
        assert!(bottom.contains("↑/↓"));
        compact.key(key(KeyCode::Up)).unwrap();
        let moved = compact.render().unwrap();
        assert!(moved.contains("PASTE 11-11/12"));
    }

    #[test]
    fn messages_entered_while_side_starts_are_buffered_and_never_sent_to_main() {
        let mut harness =
            TuiHarness::from_unified_diff("side-race", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "persistent main context");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.stream_next_response(&["main answer"]).unwrap();

        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "question typed while the fork is opening");
        harness.key(key(KeyCode::Enter)).unwrap();

        let commands = harness.agent_commands();
        assert!(commands.iter().any(|command| command.contains("StartSide")));
        assert!(commands.iter().any(|command| command.contains("SendSide")));
        assert!(!commands
            .iter()
            .skip(1)
            .any(|command| command.starts_with("Send(")));
        let starting = harness.render().unwrap();
        assert!(starting.contains("SIDE STARTING"));
        assert!(!starting.contains("question typed while the fork is opening"));

        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        let side = harness.render().unwrap();
        assert!(side.contains("question typed while the fork is opening"));
        assert!(!side.contains("persistent main context"));

        harness
            .inject_side_exited("main-session", "side-session")
            .unwrap();
        let main = harness.render().unwrap();
        assert!(main.contains("persistent main context"));
        assert!(!main.contains("question typed while the fork is opening"));
    }

    #[test]
    fn stale_main_events_cannot_mutate_the_visible_side_transcript() {
        let mut harness =
            TuiHarness::from_unified_diff("lane-filter", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();

        harness
            .inject_laned_agent_event(
                AgentLane::Main,
                AgentEvent::ResponseStarted {
                    outbound_id: "stale-main".into(),
                    outbound: crate::copilot::OutboundKind::Chat,
                    first_delta: "STALE MAIN CONTENT".into(),
                },
            )
            .unwrap();
        let after_main = harness.render().unwrap();
        assert!(!after_main.contains("STALE MAIN CONTENT"));

        harness
            .inject_laned_agent_event(
                AgentLane::Side {
                    id: "side-session".into(),
                },
                AgentEvent::ResponseStarted {
                    outbound_id: "side-turn".into(),
                    outbound: crate::copilot::OutboundKind::Chat,
                    first_delta: "VISIBLE SIDE CONTENT".into(),
                },
            )
            .unwrap();
        assert!(harness.render().unwrap().contains("VISIBLE SIDE CONTENT"));
    }

    #[test]
    fn fake_streaming_targets_main_and_side_lanes_explicitly() {
        let mut harness =
            TuiHarness::from_unified_diff("lane-streams", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "main background work");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "side foreground work");
        harness.key(key(KeyCode::Enter)).unwrap();

        harness
            .start_next_response_on_lane(AgentLane::Main, "PARKED MAIN STREAM")
            .unwrap();
        assert!(!harness
            .state
            .chat
            .iter()
            .any(|entry| entry.text.contains("PARKED MAIN STREAM")));
        assert!(harness.state.main_chat.as_ref().is_some_and(|chat| chat
            .iter()
            .any(|entry| entry.text.contains("PARKED MAIN STREAM"))));
        harness.complete_response(false).unwrap();

        harness
            .start_next_response_on_lane(
                AgentLane::Side {
                    id: "side-session".into(),
                },
                "VISIBLE SIDE STREAM",
            )
            .unwrap();
        assert!(harness
            .state
            .chat
            .iter()
            .any(|entry| entry.text.contains("VISIBLE SIDE STREAM")));
    }

    #[test]
    fn leaving_side_preserves_main_requests_that_are_still_queued() {
        let mut harness =
            TuiHarness::from_unified_diff("main-queue", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "main request waiting behind the side fork");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side isolated");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .inject_side_started("main-session", "side-session")
            .unwrap();
        harness
            .inject_side_exited("main-session", "side-session")
            .unwrap();

        let main = harness.render().unwrap();
        assert!(main.contains("main request waiting behind the side fork"));
        assert!(main.contains("queued"));
        assert_eq!(
            harness.state.agent_progress.queue_depth,
            harness.state.queue_entry_ids().len()
        );
    }

    #[test]
    fn ctrl_c_during_side_creation_requests_abort_and_exit() {
        let mut harness =
            TuiHarness::from_unified_diff("side-cancel", workflow_diff(), 100, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char('i'))).unwrap();
        type_text(&mut harness, "/side cancel this fork");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness
            .key(KeyEvent::new(
                KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ))
            .unwrap();

        let commands = harness.agent_commands();
        assert!(commands.iter().any(|command| command == "CancelSide"));
        assert!(!commands.iter().any(|command| command == "Abort"));
        assert!(harness
            .render()
            .unwrap()
            .contains("Cancelling SIDE creation"));
    }

    #[test]
    fn markdown_history_and_minimum_terminal_state_have_inspectable_frames() {
        let mut harness =
            TuiHarness::from_unified_diff("markdown", workflow_diff(), 80, 24).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness
            .inject_agent_event(AgentEvent::HistoryLoaded(vec![HistoryEntry {
                role: "copilot".into(),
                text: "# Result\n\n- **safe** item\n- [guide](https://example.invalid)\n\n| Check | Status |\n| :--- | ---: |\n| `api` | ready |\n\n> > nested\n\n```rust\nfn main() {}\n```".into(),
            }]))
            .unwrap();
        let markdown = harness.render().unwrap();
        assert!(markdown.contains("Result"));
        assert!(!markdown.contains("# Result"));
        assert!(markdown.contains("• safe item"));
        assert!(markdown.contains("guide↗"));
        assert!(markdown.contains("│ Check │ Status │"));
        assert!(markdown.contains("│ │ nested"));
        assert!(markdown.contains("fn main()"));
        assert!(!markdown.contains("**safe**"));

        harness.resize(33, 9);
        let tiny = harness.render().unwrap();
        assert!(tiny.contains("needs at"));
        assert!(tiny.contains("40×9"));
    }
}

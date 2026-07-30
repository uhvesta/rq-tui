//! Headless helpers for exercising the complete TUI reducer/effect/render loop.

use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{Context, Result};
use crossterm::event::KeyEvent;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;
use tempfile::TempDir;

use crate::app::{AppState, Effect, InputMode, Screen};
use crate::config::AppPaths;
use crate::copilot::{
    ActivityKind, AgentActivity, AgentCommand, AgentEvent, AgentEventEnvelope, AgentLane,
    AgentSink, HistoryEntry, LaneEvent, Outbound,
};
use crate::diff::{parse_unified, DiffSet};
use crate::domain::{BaseBranchSource, DeliveryState, Repo, Version, VersionKind, WorkItem};
use crate::highlight::PlainHighlighter;
use crate::storage::Storage;
use crate::ui::{handle_agent_event, handle_effect, handle_effect_failure, render};
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
        Ok(Self {
            state,
            storage,
            paths,
            agent: FakeAgent::default(),
            effects: Vec::new(),
            last_yank: None,
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
            if let Err(error) = handle_effect(
                &mut self.state,
                &self.storage,
                &self.paths,
                &self.agent,
                effect.clone(),
            ) {
                handle_effect_failure(&mut self.state, &effect, &error);
            }
        }
        Ok(descriptions)
    }

    /// Injects a successful streamed response for the oldest queued outbound.
    pub fn stream_next_response(&mut self, chunks: &[&str]) -> Result<()> {
        let outbound = self
            .agent
            .pending
            .lock()
            .expect("agent lock")
            .pop_front()
            .context("no queued fake-agent outbound")?;
        let mut chunks = chunks.iter();
        handle_agent_event(
            &mut self.state,
            &self.storage,
            AgentEvent::ResponseStarted {
                outbound_id: outbound.id.clone(),
                outbound: outbound.kind.clone(),
                first_delta: chunks.next().copied().unwrap_or_default().to_owned(),
            },
        )?;
        for chunk in chunks {
            handle_agent_event(
                &mut self.state,
                &self.storage,
                AgentEvent::ResponseDelta {
                    outbound_id: outbound.id.clone(),
                    delta: (*chunk).to_owned(),
                },
            )?;
        }
        handle_agent_event(
            &mut self.state,
            &self.storage,
            AgentEvent::ResponseComplete {
                outbound_id: outbound.id,
                aborted: false,
            },
        )
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
        state.pending_comment_ids = self
            .storage
            .pending_comment_delivery_ids(&state.work_item.item.id)?;
        state.pending_context = self
            .storage
            .context_for_work_item(&state.work_item.item.id)?
            .is_some_and(|context| context.delivery_state == DeliveryState::Pending);
        if !state.pending_asks.is_empty()
            || !state.pending_comment_ids.is_empty()
            || state.pending_context
        {
            state.screen = Screen::Recovery;
            state.status = "Outbound delivery is uncertain; resend or discard explicitly".into();
        }
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
                },
                activity: None,
            },
        )
    }

    pub fn backdate_agent_progress(&mut self, age: std::time::Duration) {
        let now = std::time::Instant::now();
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
                .map(|column| buffer[(column, row)].symbol())
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
            "review", "command", "composer", "quiet", "side", "markdown", "tiny",
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
        "side" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            press(&mut harness, crossterm::event::KeyCode::Char('i'))?;
            type_into(&mut harness, "/side What does this function guarantee?")?;
            press(&mut harness, crossterm::event::KeyCode::Enter)?;
            harness.inject_side_started("main-session", "side-session")?;
        }
        "markdown" => {
            press(&mut harness, crossterm::event::KeyCode::Tab)?;
            harness.inject_agent_event(AgentEvent::HistoryLoaded(vec![HistoryEntry {
                role: "copilot".into(),
                text: "# Review result\n\n- **Safe** path\n- `retry` is bounded\n\n```rust\nfn review() -> Result<()> {\n    Ok(())\n}\n```".into(),
            }]))?;
        }
        "tiny" => {
            harness.resize(width.min(20), height.min(5));
        }
        other => anyhow::bail!(
            "unknown UI snapshot state {other:?}; use review, command, composer, quiet, side, markdown, tiny, or all"
        ),
    }
    harness.render()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::TuiHarness;
    use crate::app::tests_support::state_for_ui;
    use crate::copilot::{ActivityKind, AgentEvent, AgentLane, HistoryEntry};

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
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        assert_eq!(harness.mode(), "VISUAL");
        assert!(harness.selected_cell_count().unwrap() > 100);
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

        let mut visual = TuiHarness::from_unified_diff("visual", workflow_diff(), 110, 28).unwrap();
        visual.key(key(KeyCode::Char('j'))).unwrap();
        visual.key(key(KeyCode::Char('v'))).unwrap();
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
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        harness.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(
            harness.last_yank(),
            Some("fn review() {\n    let added = true;\n    use_added();")
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
    fn file_switch_clears_visual_mode_and_split_unified_render_selection() {
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
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        let split_selected = harness.selected_cell_count().unwrap();
        assert!(split_selected > 0);

        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "diff unified");
        harness.key(key(KeyCode::Enter)).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('j'))).unwrap();
        assert!(harness.selected_cell_count().unwrap() > 0);

        harness.key(key(KeyCode::Esc)).unwrap();
        harness.key(key(KeyCode::Char('l'))).unwrap();
        harness.key(key(KeyCode::Char('v'))).unwrap();
        harness.key(key(KeyCode::Char('l'))).unwrap();
        assert_eq!(harness.mode(), "NORMAL");
        assert!(harness.render().unwrap().contains("src/two.rs"));
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
    fn visual_search_extends_the_fixed_anchor_and_chat_visual_yanks_messages() {
        let mut review = TuiHarness::from_unified_diff("search", workflow_diff(), 100, 24).unwrap();
        review.key(key(KeyCode::Char('v'))).unwrap();
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
        chat.key(key(KeyCode::Char('v'))).unwrap();
        chat.key(key(KeyCode::Char('k'))).unwrap();
        assert!(chat.render().unwrap().contains("VISUAL  messages 1-2"));
        chat.key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(chat.last_yank(), Some("hello\nstreamed answer"));
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
        ask.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(ask.mode(), "INSERT");
        type_text(&mut ask, "follow-up question");
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
        review.key(key(KeyCode::Char('v'))).unwrap();
        review.key(key(KeyCode::PageDown)).unwrap();
        assert_eq!(review.mode(), "VISUAL");
        assert!(review.render().unwrap().contains("VISUAL  rows 1-"));
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
    fn command_palette_is_scrollable_selectable_and_unmistakably_modal() {
        let mut harness =
            TuiHarness::from_unified_diff("commands", workflow_diff(), 92, 22).unwrap();
        harness.key(key(KeyCode::Tab)).unwrap();
        harness.key(key(KeyCode::Char(':'))).unwrap();
        let initial = harness.render().unwrap();
        assert!(initial.contains("COMMAND MODE · Command palette"));
        assert!(initial.contains("COMMAND MODE ACTIVE"));
        assert!(initial.contains("↑/↓ select"));

        for _ in 0..12 {
            harness.key(key(KeyCode::Down)).unwrap();
        }
        let scrolled = harness.render().unwrap();
        assert!(scrolled.contains(":snapshot") || scrolled.contains(":generate-context"));
        harness.key(key(KeyCode::Tab)).unwrap();
        assert!(harness.render().unwrap().contains(":"));
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
        assert!(composing.contains("CHAT INPUT · INSERT"));
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
        harness.key(key(KeyCode::Up)).unwrap();
        harness.key(key(KeyCode::Up)).unwrap();
        harness.key(key(KeyCode::Up)).unwrap();
        assert!(harness.chat_scroll() < live_bottom);
        let paused = harness.render().unwrap();
        assert!(paused.contains("rows "));
        harness.key(key(KeyCode::Char('G'))).unwrap();
        harness.render().unwrap();
        assert!(harness.chat_scroll() >= live_bottom);
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

        harness.key(key(KeyCode::Char(':'))).unwrap();
        type_text(&mut harness, "agent-status");
        harness.key(key(KeyCode::Enter)).unwrap();
        let diagnostics = harness.render().unwrap();
        assert!(diagnostics.contains("COPILOT SDK LIVENESS"));
        assert!(diagnostics.contains("Running security-review skill"));
        assert!(diagnostics.contains("last SDK event"));
        assert!(diagnostics.contains("outbound id"));
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
        assert!(commands.iter().any(|command| command == "Abort"));
        assert!(commands.iter().any(|command| command == "ExitSide"));
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
                text: "# Result\n\n- **safe** item\n\n```rust\nfn main() {}\n```".into(),
            }]))
            .unwrap();
        let markdown = harness.render().unwrap();
        assert!(markdown.contains("# Result"));
        assert!(markdown.contains("• safe item"));
        assert!(markdown.contains("fn main()"));
        assert!(!markdown.contains("**safe**"));

        harness.resize(20, 5);
        let tiny = harness.render().unwrap();
        assert!(tiny.contains("needs at"));
        assert!(tiny.contains("32×8"));
    }
}

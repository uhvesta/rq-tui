//! Bounded, lane-owned runtime state for agent activity.
//!
//! This module deliberately has no dependency on the UI or Copilot bridge. It
//! is the small state machine those layers can share while they migrate away
//! from one global progress object.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_QUEUE: usize = 64;
const MAX_TIMELINE: usize = 24;
const MAX_FAILED: usize = 24;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum AgentLane {
    #[default]
    Main,
    Side,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VisibleLane {
    #[default]
    Main,
    Side,
}

impl VisibleLane {
    pub(crate) fn as_lane(self) -> AgentLane {
        match self {
            Self::Main => AgentLane::Main,
            Self::Side => AgentLane::Side,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LaneConnection {
    Connecting,
    Connected,
    #[default]
    Disconnected,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LaneLifecycle {
    #[default]
    Idle,
    Running,
    Stopping,
    Failed,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimelineEntry {
    pub(crate) at: Instant,
    pub(crate) phase: AgentPhase,
    pub(crate) summary: String,
    pub(crate) detail: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentProgress {
    pub(crate) phase: AgentPhase,
    pub(crate) summary: String,
    pub(crate) detail: String,
    pub(crate) turn_started_at: Option<Instant>,
    pub(crate) last_event_at: Instant,
    pub(crate) event_count: usize,
    pub(crate) timeline: VecDeque<TimelineEntry>,
}

impl AgentProgress {
    fn new(now: Instant) -> Self {
        Self {
            phase: AgentPhase::Connecting,
            summary: "Starting agent session".into(),
            detail: "Waiting for the session-ready event".into(),
            turn_started_at: None,
            last_event_at: now,
            event_count: 0,
            timeline: VecDeque::new(),
        }
    }

    fn record(
        &mut self,
        now: Instant,
        phase: AgentPhase,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let summary = summary.into();
        let detail = detail.into();
        if phase.is_active() && !self.phase.is_active() {
            self.turn_started_at = Some(now);
        } else if !phase.is_active() {
            self.turn_started_at = None;
        }
        self.phase = phase;
        self.summary = summary.clone();
        self.detail = detail.clone();
        self.last_event_at = now;
        self.event_count = self.event_count.saturating_add(1);
        self.timeline.push_back(TimelineEntry {
            at: now,
            phase,
            summary,
            detail,
        });
        while self.timeline.len() > MAX_TIMELINE {
            self.timeline.pop_front();
        }
    }

    fn disconnected(&mut self, now: Instant, detail: impl Into<String>) {
        let detail = detail.into();
        self.phase = AgentPhase::Disconnected;
        self.summary = "Agent disconnected".into();
        self.detail = detail.clone();
        self.turn_started_at = None;
        self.event_count = self.event_count.saturating_add(1);
        self.timeline.push_back(TimelineEntry {
            at: now,
            phase: AgentPhase::Disconnected,
            summary: self.summary.clone(),
            detail,
        });
        while self.timeline.len() > MAX_TIMELINE {
            self.timeline.pop_front();
        }
        // `last_event_at` intentionally remains the last SDK event. A local
        // disconnect observation must not make a stale session look healthy.
    }

    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        self.turn_started_at
            .map(|started| now.saturating_duration_since(started))
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SideGeneration {
    pub(crate) session_id: String,
    pub(crate) generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FailedTurn {
    pub(crate) outbound_id: String,
    pub(crate) message: String,
    pub(crate) at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueLabel {
    Active,
    Queued,
    Stopping,
    Failed,
    Unknown,
}

impl QueueLabel {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Queued => "QUEUED",
            Self::Stopping => "STOPPING",
            Self::Failed => "FAILED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LaneState {
    lane: AgentLane,
    connection: LaneConnection,
    lifecycle: LaneLifecycle,
    progress: AgentProgress,
    active_outbound_id: Option<String>,
    stopping_outbound_id: Option<String>,
    queued_ids: VecDeque<String>,
    failed_turns: VecDeque<FailedTurn>,
    side_generation: Option<SideGeneration>,
    disconnected_at: Option<Instant>,
}

impl LaneState {
    fn new(lane: AgentLane, now: Instant) -> Self {
        let connection = match lane {
            AgentLane::Main => LaneConnection::Connecting,
            AgentLane::Side => LaneConnection::Disconnected,
        };
        Self {
            lane,
            connection,
            lifecycle: LaneLifecycle::Idle,
            progress: AgentProgress::new(now),
            active_outbound_id: None,
            stopping_outbound_id: None,
            queued_ids: VecDeque::new(),
            failed_turns: VecDeque::new(),
            side_generation: None,
            disconnected_at: None,
        }
    }

    pub(crate) fn lane(&self) -> AgentLane {
        self.lane
    }

    pub(crate) fn connection(&self) -> LaneConnection {
        self.connection
    }

    pub(crate) fn lifecycle(&self) -> LaneLifecycle {
        self.lifecycle
    }

    pub(crate) fn progress(&self) -> &AgentProgress {
        &self.progress
    }

    pub(crate) fn active_outbound_id(&self) -> Option<&str> {
        self.active_outbound_id.as_deref()
    }

    pub(crate) fn queued_ids(&self) -> impl DoubleEndedIterator<Item = &String> {
        self.queued_ids.iter()
    }

    pub(crate) fn failed_turns(&self) -> impl DoubleEndedIterator<Item = &FailedTurn> {
        self.failed_turns.iter()
    }

    pub(crate) fn side_generation(&self) -> Option<&SideGeneration> {
        self.side_generation.as_ref()
    }

    pub(crate) fn disconnected_at(&self) -> Option<Instant> {
        self.disconnected_at
    }

    fn accepts_generation(&self, generation: Option<&SideGeneration>) -> bool {
        match (&self.lane, generation) {
            (AgentLane::Main, None) => true,
            (AgentLane::Main, Some(_)) => false,
            (AgentLane::Side, Some(candidate)) => self.side_generation.as_ref() == Some(candidate),
            (AgentLane::Side, None) => false,
        }
    }

    fn accepts_outbound(&self, outbound_id: Option<&str>) -> bool {
        outbound_id.is_none_or(|id| self.active_outbound_id() == Some(id))
    }

    fn record_activity(
        &mut self,
        now: Instant,
        outbound_id: Option<&str>,
        phase: AgentPhase,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) -> bool {
        if !self.accepts_outbound(outbound_id) {
            return false;
        }
        self.progress.record(now, phase, summary, detail);
        self.lifecycle = match phase {
            AgentPhase::Stopping => LaneLifecycle::Stopping,
            AgentPhase::Failed => LaneLifecycle::Failed,
            AgentPhase::Idle | AgentPhase::Disconnected => LaneLifecycle::Idle,
            _ => LaneLifecycle::Running,
        };
        true
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AgentState {
    main: LaneState,
    side: LaneState,
    visible_lane: VisibleLane,
    next_side_generation: u64,
}

impl AgentState {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            main: LaneState::new(AgentLane::Main, now),
            side: LaneState::new(AgentLane::Side, now),
            visible_lane: VisibleLane::Main,
            next_side_generation: 0,
        }
    }

    pub(crate) fn visible_lane(&self) -> VisibleLane {
        self.visible_lane
    }

    pub(crate) fn set_visible_lane(&mut self, lane: VisibleLane) {
        self.visible_lane = lane;
    }

    pub(crate) fn lane(&self, lane: AgentLane) -> &LaneState {
        match lane {
            AgentLane::Main => &self.main,
            AgentLane::Side => &self.side,
        }
    }

    pub(crate) fn main(&self) -> &LaneState {
        &self.main
    }

    pub(crate) fn side(&self) -> &LaneState {
        &self.side
    }

    pub(crate) fn visible(&self) -> &LaneState {
        self.lane(self.visible_lane.as_lane())
    }

    pub(crate) fn connect(&mut self, lane: AgentLane, now: Instant) {
        let state = self.lane_mut(lane);
        state.connection = LaneConnection::Connected;
        state.disconnected_at = None;
        state.record_activity(
            now,
            None,
            AgentPhase::Idle,
            "Agent ready",
            "Connected and accepting requests",
        );
    }

    pub(crate) fn begin_side(
        &mut self,
        session_id: impl Into<String>,
        now: Instant,
    ) -> SideGeneration {
        self.next_side_generation = self.next_side_generation.saturating_add(1);
        let token = SideGeneration {
            session_id: session_id.into(),
            generation: self.next_side_generation,
        };
        self.side = LaneState::new(AgentLane::Side, now);
        self.side.side_generation = Some(token.clone());
        self.side.connection = LaneConnection::Connected;
        self.side.record_activity(
            now,
            None,
            AgentPhase::Idle,
            "SIDE ready",
            "Side session is accepting requests",
        );
        token
    }

    pub(crate) fn end_side(&mut self, generation: &SideGeneration, now: Instant) -> bool {
        if self.side.side_generation.as_ref() != Some(generation) {
            return false;
        }
        self.side.connection = LaneConnection::Disconnected;
        self.side.disconnected_at = Some(now);
        self.side.progress.disconnected(now, "Side session ended");
        self.side.lifecycle = LaneLifecycle::Idle;
        true
    }

    pub(crate) fn enqueue(
        &mut self,
        lane: AgentLane,
        outbound_id: impl Into<String>,
        now: Instant,
    ) -> bool {
        self.enqueue_for(lane, None, outbound_id, now)
    }

    pub(crate) fn enqueue_side(
        &mut self,
        generation: &SideGeneration,
        outbound_id: impl Into<String>,
        now: Instant,
    ) -> bool {
        self.enqueue_for(AgentLane::Side, Some(generation), outbound_id, now)
    }

    fn enqueue_for(
        &mut self,
        lane: AgentLane,
        generation: Option<&SideGeneration>,
        outbound_id: impl Into<String>,
        now: Instant,
    ) -> bool {
        let outbound_id = outbound_id.into();
        let state = self.lane_mut(lane);
        if !state.accepts_generation(generation)
            || state.active_outbound_id() == Some(outbound_id.as_str())
            || state.queued_ids.iter().any(|id| id == &outbound_id)
            || state
                .failed_turns
                .iter()
                .any(|turn| turn.outbound_id == outbound_id)
            || state.queued_ids.len() >= MAX_QUEUE
        {
            return false;
        }
        state.queued_ids.push_back(outbound_id);
        if state.active_outbound_id.is_none() {
            state.progress.record(
                now,
                AgentPhase::Queued,
                "Request queued",
                "Waiting for the agent turn to start",
            );
            state.lifecycle = LaneLifecycle::Running;
        }
        true
    }

    pub(crate) fn start_next(&mut self, lane: AgentLane, now: Instant) -> Option<String> {
        self.start_next_for(lane, None, now)
    }

    pub(crate) fn start_next_side(
        &mut self,
        generation: &SideGeneration,
        now: Instant,
    ) -> Option<String> {
        self.start_next_for(AgentLane::Side, Some(generation), now)
    }

    fn start_next_for(
        &mut self,
        lane: AgentLane,
        generation: Option<&SideGeneration>,
        now: Instant,
    ) -> Option<String> {
        let state = self.lane_mut(lane);
        if !state.accepts_generation(generation) || state.active_outbound_id.is_some() {
            return None;
        }
        let outbound_id = state.queued_ids.pop_front()?;
        state.active_outbound_id = Some(outbound_id.clone());
        state.stopping_outbound_id = None;
        state.progress.record(
            now,
            AgentPhase::Planning,
            "Working",
            "Planning the next response",
        );
        state.lifecycle = LaneLifecycle::Running;
        Some(outbound_id)
    }

    pub(crate) fn request_stop(&mut self, lane: AgentLane, now: Instant) -> Option<String> {
        self.request_stop_for(lane, None, now)
    }

    pub(crate) fn request_stop_side(
        &mut self,
        generation: &SideGeneration,
        now: Instant,
    ) -> Option<String> {
        self.request_stop_for(AgentLane::Side, Some(generation), now)
    }

    fn request_stop_for(
        &mut self,
        lane: AgentLane,
        generation: Option<&SideGeneration>,
        now: Instant,
    ) -> Option<String> {
        let state = self.lane_mut(lane);
        if !state.accepts_generation(generation) {
            return None;
        }
        let outbound_id = state.active_outbound_id.clone()?;
        state.stopping_outbound_id = Some(outbound_id.clone());
        state.progress.record(
            now,
            AgentPhase::Stopping,
            "Stopping",
            "Cancellation requested; waiting for the agent to settle",
        );
        state.lifecycle = LaneLifecycle::Stopping;
        Some(outbound_id)
    }

    pub(crate) fn complete(&mut self, lane: AgentLane, outbound_id: &str, now: Instant) -> bool {
        self.complete_for(lane, None, outbound_id, now)
    }

    pub(crate) fn complete_side(
        &mut self,
        generation: &SideGeneration,
        outbound_id: &str,
        now: Instant,
    ) -> bool {
        self.complete_for(AgentLane::Side, Some(generation), outbound_id, now)
    }

    fn complete_for(
        &mut self,
        lane: AgentLane,
        generation: Option<&SideGeneration>,
        outbound_id: &str,
        now: Instant,
    ) -> bool {
        let state = self.lane_mut(lane);
        if !state.accepts_generation(generation) || state.active_outbound_id() != Some(outbound_id)
        {
            return false;
        }
        state.active_outbound_id = None;
        state.stopping_outbound_id = None;
        state.lifecycle = LaneLifecycle::Idle;
        state.progress.record(
            now,
            AgentPhase::Idle,
            "Turn complete",
            "Ready for the next request",
        );
        true
    }

    pub(crate) fn fail(
        &mut self,
        lane: AgentLane,
        outbound_id: &str,
        message: impl Into<String>,
        now: Instant,
    ) -> bool {
        self.fail_for(lane, None, outbound_id, message, now)
    }

    pub(crate) fn fail_side(
        &mut self,
        generation: &SideGeneration,
        outbound_id: &str,
        message: impl Into<String>,
        now: Instant,
    ) -> bool {
        self.fail_for(AgentLane::Side, Some(generation), outbound_id, message, now)
    }

    fn fail_for(
        &mut self,
        lane: AgentLane,
        generation: Option<&SideGeneration>,
        outbound_id: &str,
        message: impl Into<String>,
        now: Instant,
    ) -> bool {
        let state = self.lane_mut(lane);
        if !state.accepts_generation(generation) || state.active_outbound_id() != Some(outbound_id)
        {
            return false;
        }
        let message = message.into();
        state.active_outbound_id = None;
        state.stopping_outbound_id = None;
        state.lifecycle = LaneLifecycle::Failed;
        state.failed_turns.push_back(FailedTurn {
            outbound_id: outbound_id.into(),
            message: message.clone(),
            at: now,
        });
        while state.failed_turns.len() > MAX_FAILED {
            state.failed_turns.pop_front();
        }
        state
            .progress
            .record(now, AgentPhase::Failed, "Turn failed", message);
        true
    }

    pub(crate) fn queue_label(&self, lane: AgentLane, outbound_id: &str) -> QueueLabel {
        let state = self.lane(lane);
        if state.active_outbound_id() == Some(outbound_id) {
            if state.stopping_outbound_id.as_deref() == Some(outbound_id) {
                QueueLabel::Stopping
            } else {
                QueueLabel::Active
            }
        } else if state.queued_ids.iter().any(|id| id == outbound_id) {
            QueueLabel::Queued
        } else if state
            .failed_turns
            .iter()
            .any(|turn| turn.outbound_id == outbound_id)
        {
            QueueLabel::Failed
        } else {
            QueueLabel::Unknown
        }
    }

    pub(crate) fn record_activity(
        &mut self,
        lane: AgentLane,
        outbound_id: Option<&str>,
        phase: AgentPhase,
        summary: impl Into<String>,
        detail: impl Into<String>,
        now: Instant,
    ) -> bool {
        self.lane_mut(lane)
            .record_activity(now, outbound_id, phase, summary, detail)
    }

    pub(crate) fn record_activity_side(
        &mut self,
        generation: &SideGeneration,
        outbound_id: Option<&str>,
        phase: AgentPhase,
        summary: impl Into<String>,
        detail: impl Into<String>,
        now: Instant,
    ) -> bool {
        let state = self.lane_mut(AgentLane::Side);
        state.accepts_generation(Some(generation))
            && state.record_activity(now, outbound_id, phase, summary, detail)
    }

    pub(crate) fn disconnect_all(&mut self, now: Instant, detail: impl Into<String>) {
        let detail = detail.into();
        for state in [&mut self.main, &mut self.side] {
            state.connection = LaneConnection::Disconnected;
            state.disconnected_at = Some(now);
            state.progress.disconnected(now, detail.clone());
        }
    }
}

impl AgentState {
    fn lane_mut(&mut self, lane: AgentLane) -> &mut LaneState {
        match lane {
            AgentLane::Main => &mut self.main,
            AgentLane::Side => &mut self.side,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    #[test]
    fn hidden_main_activity_is_retained_when_side_is_visible() {
        let mut state = AgentState::new(at(0));
        state.connect(AgentLane::Main, at(1));
        let side = state.begin_side("side-session", at(2));
        state.set_visible_lane(VisibleLane::Side);
        assert!(state.enqueue(AgentLane::Main, "main-1", at(3)));
        assert_eq!(
            state.start_next(AgentLane::Main, at(4)).as_deref(),
            Some("main-1")
        );
        assert!(state.record_activity(
            AgentLane::Main,
            Some("main-1"),
            AgentPhase::Tool,
            "Running tool",
            "Inspecting the repository",
            at(5),
        ));
        assert_eq!(state.visible_lane(), VisibleLane::Side);
        assert_eq!(state.visible().lane(), AgentLane::Side);
        assert_eq!(state.main().progress().phase, AgentPhase::Tool);
        assert_eq!(state.main().active_outbound_id(), Some("main-1"));
        assert_eq!(state.side().side_generation(), Some(&side));
    }

    #[test]
    fn side_failure_does_not_change_main_progress() {
        let mut state = AgentState::new(at(0));
        state.connect(AgentLane::Main, at(1));
        assert!(state.enqueue(AgentLane::Main, "main-1", at(2)));
        assert_eq!(
            state.start_next(AgentLane::Main, at(3)).as_deref(),
            Some("main-1")
        );
        state.record_activity(
            AgentLane::Main,
            Some("main-1"),
            AgentPhase::Responding,
            "Responding",
            "Streaming answer",
            at(4),
        );
        let main_phase = state.main().progress().phase;
        let main_last_event = state.main().progress().last_event_at;
        let side = state.begin_side("side-session", at(5));
        assert!(state.enqueue_side(&side, "side-1", at(6)));
        assert_eq!(
            state.start_next_side(&side, at(7)).as_deref(),
            Some("side-1")
        );
        assert!(state.fail_side(&side, "side-1", "tool crashed", at(8)));
        assert_eq!(state.main().progress().phase, main_phase);
        assert_eq!(state.main().progress().last_event_at, main_last_event);
        assert_eq!(state.side().lifecycle(), LaneLifecycle::Failed);
        assert_eq!(
            state
                .side()
                .failed_turns()
                .next_back()
                .map(|turn| turn.message.as_str()),
            Some("tool crashed")
        );
        assert_eq!(
            state.queue_label(AgentLane::Side, "side-1"),
            QueueLabel::Failed
        );
    }

    #[test]
    fn queue_labels_are_active_queued_stopping_and_failed() {
        let mut state = AgentState::new(at(0));
        state.connect(AgentLane::Main, at(1));
        assert!(state.enqueue(AgentLane::Main, "active", at(2)));
        assert!(state.enqueue(AgentLane::Main, "queued", at(3)));
        assert_eq!(
            state.start_next(AgentLane::Main, at(4)).as_deref(),
            Some("active")
        );
        assert_eq!(
            state.queue_label(AgentLane::Main, "active"),
            QueueLabel::Active
        );
        assert_eq!(
            state.queue_label(AgentLane::Main, "queued"),
            QueueLabel::Queued
        );
        assert_eq!(
            state.request_stop(AgentLane::Main, at(5)).as_deref(),
            Some("active")
        );
        assert_eq!(
            state.queue_label(AgentLane::Main, "active"),
            QueueLabel::Stopping
        );
        assert!(state.fail(AgentLane::Main, "active", "cancelled", at(6)));
        assert_eq!(
            state.queue_label(AgentLane::Main, "active"),
            QueueLabel::Failed
        );
        assert_eq!(
            state.queue_label(AgentLane::Main, "missing"),
            QueueLabel::Unknown
        );
        assert_eq!(QueueLabel::Failed.label(), "FAILED");
    }

    #[test]
    fn side_events_from_old_generation_are_rejected() {
        let mut state = AgentState::new(at(0));
        let old = state.begin_side("same-session", at(1));
        assert!(state.enqueue_side(&old, "old-turn", at(2)));
        assert_eq!(
            state.start_next_side(&old, at(3)).as_deref(),
            Some("old-turn")
        );
        let current = state.begin_side("same-session", at(4));
        assert_ne!(old, current);
        assert!(!state.record_activity_side(
            &old,
            Some("old-turn"),
            AgentPhase::Tool,
            "Stale tool",
            "Must not appear",
            at(5),
        ));
        assert!(!state.fail_side(&old, "old-turn", "late failure", at(6)));
        assert_eq!(state.side().progress().phase, AgentPhase::Idle);
        assert_eq!(state.side().active_outbound_id(), None);
        assert!(state.enqueue_side(&current, "new-turn", at(7)));
    }

    #[test]
    fn disconnect_propagates_and_preserves_last_sdk_timestamps() {
        let mut state = AgentState::new(at(0));
        state.connect(AgentLane::Main, at(1));
        let side = state.begin_side("side-session", at(2));
        let main_activity_at = at(3);
        state.record_activity(
            AgentLane::Main,
            None,
            AgentPhase::Responding,
            "Streaming",
            "Receiving tokens",
            main_activity_at,
        );
        assert_eq!(
            state
                .main()
                .progress()
                .elapsed(main_activity_at + Duration::from_secs(7)),
            Duration::from_secs(7)
        );
        state.record_activity_side(
            &side,
            None,
            AgentPhase::Tool,
            "Tool",
            "Running command",
            at(4),
        );
        let main_last = state.main().progress().last_event_at;
        let side_last = state.side().progress().last_event_at;
        let disconnected_at = at(10);
        state.disconnect_all(disconnected_at, "event stream closed");
        for lane in [state.main(), state.side()] {
            assert_eq!(lane.connection(), LaneConnection::Disconnected);
            assert_eq!(lane.disconnected_at(), Some(disconnected_at));
            assert_eq!(lane.progress().phase, AgentPhase::Disconnected);
        }
        assert_eq!(state.main().progress().last_event_at, main_last);
        assert_eq!(state.side().progress().last_event_at, side_last);
    }

    #[test]
    fn successful_main_and_side_turns_settle_and_side_generation_closes() {
        let mut state = AgentState::new(at(0));
        state.connect(AgentLane::Main, at(1));
        assert!(state.enqueue(AgentLane::Main, "main-1", at(2)));
        assert_eq!(
            state.start_next(AgentLane::Main, at(3)).as_deref(),
            Some("main-1")
        );
        assert!(state.complete(AgentLane::Main, "main-1", at(4)));
        assert_eq!(state.main().lifecycle(), LaneLifecycle::Idle);

        let side = state.begin_side("side-session", at(5));
        assert!(state.enqueue_side(&side, "side-1", at(6)));
        assert_eq!(
            state.start_next_side(&side, at(7)).as_deref(),
            Some("side-1")
        );
        assert_eq!(
            state.request_stop_side(&side, at(8)).as_deref(),
            Some("side-1")
        );
        assert!(state.complete_side(&side, "side-1", at(9)));
        assert!(state.end_side(&side, at(10)));
        assert_eq!(state.side().connection(), LaneConnection::Disconnected);
        assert_eq!(state.side().lifecycle(), LaneLifecycle::Idle);
        assert!(!state.end_side(
            &SideGeneration {
                session_id: "stale".into(),
                generation: side.generation,
            },
            at(11)
        ));
    }

    #[test]
    fn queue_and_timeline_are_bounded() {
        let mut state = AgentState::new(at(0));
        state.connect(AgentLane::Main, at(1));
        for i in 0..(MAX_QUEUE + 4) {
            assert_eq!(
                state.enqueue(AgentLane::Main, format!("q-{i}"), at(i as u64 + 2)),
                i < MAX_QUEUE
            );
        }
        assert_eq!(state.main().queued_ids().count(), MAX_QUEUE);
        for i in 0..(MAX_TIMELINE + 4) {
            state.record_activity(
                AgentLane::Main,
                None,
                AgentPhase::Tool,
                format!("activity-{i}"),
                "bounded",
                at(i as u64 + 100),
            );
        }
        assert_eq!(state.main().progress().timeline.len(), MAX_TIMELINE);
    }
}

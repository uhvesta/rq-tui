//! Pure modal state and transition logic for the rq-tui application.
//!
//! This crate intentionally has no terminal, renderer, storage, or process
//! dependencies. The TUI adapts crossterm events into [`ModalKey`] and applies
//! the resulting effects to its richer state after the reducer returns.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
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
pub enum InputMode {
    #[default]
    Normal,
    Visual,
    Command,
    Search,
    Compose,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    FilePicker,
    #[default]
    Diff,
    Chat,
    InlineAsk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModalState {
    pub screen: Screen,
    pub previous_screen: Screen,
    pub input_mode: InputMode,
    pub input_return_mode: InputMode,
    pub focus: Focus,
    pub picker_open: bool,
}

impl Default for ModalState {
    fn default() -> Self {
        Self {
            screen: Screen::Review,
            previous_screen: Screen::Review,
            input_mode: InputMode::Normal,
            input_return_mode: InputMode::Normal,
            focus: Focus::Diff,
            picker_open: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalKey {
    Character(char),
    Escape,
    Tab,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalEffect {
    EnterCommand,
    EnterSearch,
    ToggleReviewChat,
    ToggleFilePicker,
    DismissVisualSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub state: ModalState,
    pub effects: Vec<ModalEffect>,
    pub handled: bool,
}

impl Transition {
    fn unhandled(state: ModalState) -> Self {
        Self {
            state,
            effects: Vec::new(),
            handled: false,
        }
    }

    fn handled(state: ModalState, effect: ModalEffect) -> Self {
        Self {
            state,
            effects: vec![effect],
            handled: true,
        }
    }
}

/// Reduce one modal key event without performing any I/O or rendering.
///
/// The reducer only claims keys whose transition is valid in the current
/// modal state. An unhandled transition lets the surrounding reducer continue
/// with pane-specific movement and editing behavior.
pub fn reduce(mut state: ModalState, key: ModalKey) -> Transition {
    match key {
        ModalKey::Character(':') if is_review_or_chat(state.screen) => {
            state.input_return_mode = state.input_mode;
            state.input_mode = InputMode::Command;
            Transition::handled(state, ModalEffect::EnterCommand)
        }
        ModalKey::Character('/') if is_review_or_chat(state.screen) => {
            state.input_return_mode = state.input_mode;
            state.input_mode = InputMode::Search;
            Transition::handled(state, ModalEffect::EnterSearch)
        }
        ModalKey::Tab
            if state.input_mode == InputMode::Normal && is_review_or_chat(state.screen) =>
        {
            state.screen = match state.screen {
                Screen::Review => Screen::Chat,
                Screen::Chat => Screen::Review,
                _ => unreachable!("screen was checked above"),
            };
            state.focus = if state.screen == Screen::Chat {
                Focus::Chat
            } else {
                Focus::Diff
            };
            Transition::handled(state, ModalEffect::ToggleReviewChat)
        }
        ModalKey::Character('t')
            if state.screen == Screen::Review
                && matches!(state.input_mode, InputMode::Normal | InputMode::Visual) =>
        {
            state.picker_open = !state.picker_open;
            state.focus = if state.picker_open {
                Focus::FilePicker
            } else {
                Focus::Diff
            };
            Transition::handled(state, ModalEffect::ToggleFilePicker)
        }
        ModalKey::Escape if state.input_mode == InputMode::Visual => {
            state.input_mode = InputMode::Normal;
            Transition::handled(state, ModalEffect::DismissVisualSelection)
        }
        _ => Transition::unhandled(state),
    }
}

fn is_review_or_chat(screen: Screen) -> bool {
    matches!(screen, Screen::Review | Screen::Chat)
}

#[cfg(test)]
mod tests {
    use super::{reduce, Focus, InputMode, ModalEffect, ModalKey, ModalState, Screen};

    #[test]
    fn command_and_search_preserve_the_return_mode() {
        let state = ModalState {
            input_mode: InputMode::Visual,
            ..ModalState::default()
        };
        let command = reduce(state, ModalKey::Character(':'));
        assert!(command.handled);
        assert_eq!(command.state.input_mode, InputMode::Command);
        assert_eq!(command.state.input_return_mode, InputMode::Visual);
        assert_eq!(command.effects, vec![ModalEffect::EnterCommand]);

        let search = reduce(state, ModalKey::Character('/'));
        assert!(search.handled);
        assert_eq!(search.state.input_mode, InputMode::Search);
        assert_eq!(search.state.input_return_mode, InputMode::Visual);
        assert_eq!(search.effects, vec![ModalEffect::EnterSearch]);
    }

    #[test]
    fn tab_switches_only_the_review_and_chat_screens() {
        let chat = reduce(
            ModalState {
                screen: Screen::Review,
                ..ModalState::default()
            },
            ModalKey::Tab,
        );
        assert!(chat.handled);
        assert_eq!(chat.state.screen, Screen::Chat);
        assert_eq!(chat.state.focus, Focus::Chat);

        let overlay = reduce(
            ModalState {
                screen: Screen::Settings,
                ..ModalState::default()
            },
            ModalKey::Tab,
        );
        assert!(!overlay.handled);
        assert_eq!(overlay.state.screen, Screen::Settings);
    }

    #[test]
    fn picker_toggle_is_reversible_and_updates_focus() {
        let opened = reduce(ModalState::default(), ModalKey::Character('t'));
        assert_eq!(opened.effects, vec![ModalEffect::ToggleFilePicker]);
        assert!(opened.state.picker_open);
        assert_eq!(opened.state.focus, Focus::FilePicker);

        let closed = reduce(opened.state, ModalKey::Character('t'));
        assert!(closed.state.input_mode == InputMode::Normal);
        assert!(!closed.state.picker_open);
        assert_eq!(closed.state.focus, Focus::Diff);
    }

    #[test]
    fn escape_exits_visual_mode_but_not_command_mode() {
        let visual = reduce(
            ModalState {
                input_mode: InputMode::Visual,
                ..ModalState::default()
            },
            ModalKey::Escape,
        );
        assert!(visual.handled);
        assert_eq!(visual.state.input_mode, InputMode::Normal);
        assert_eq!(visual.effects, vec![ModalEffect::DismissVisualSelection]);

        let command = reduce(
            ModalState {
                input_mode: InputMode::Command,
                ..ModalState::default()
            },
            ModalKey::Escape,
        );
        assert!(!command.handled);
        assert_eq!(command.state.input_mode, InputMode::Command);
    }
}

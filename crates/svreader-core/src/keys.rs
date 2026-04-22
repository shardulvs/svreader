use crate::navigator::Action;
use crate::viewport::ZoomMode;

/// A terminal-agnostic key event. The TUI layer converts crossterm
/// events into `Key` before feeding them to the parser, so everything
/// downstream is testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Esc,
    Enter,
    Tab,
    BackTab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyParserState {
    /// Accumulated count prefix (e.g. 42 in `42G`).
    pub count: Option<usize>,
    /// Pending leader char (e.g. 'g' waiting for 'g' or 't').
    pub leader: Option<char>,
}

impl KeyParserState {
    pub fn active(&self) -> bool {
        self.count.is_some() || self.leader.is_some()
    }

    pub fn clear(&mut self) {
        self.count = None;
        self.leader = None;
    }

    pub fn hint(&self) -> String {
        let mut s = String::new();
        if let Some(c) = self.count {
            s.push_str(&c.to_string());
        }
        if let Some(c) = self.leader {
            s.push(c);
        }
        s
    }
}

/// Outcome of feeding a single key into the parser.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// Nothing produced — keep accumulating. Includes "state cleared".
    Pending,
    /// Produced an action, repeated `count` times.
    Action { action: Action, count: usize },
    /// Enter command-mode (open `:` palette).
    OpenCommand,
    /// Toggle help overlay (`?`).
    ToggleHelp,
    /// Request to quit.
    Quit,
}

impl KeyOutcome {
    fn action(action: Action, count: usize) -> Self {
        KeyOutcome::Action {
            action,
            count: count.max(1),
        }
    }
}

pub struct KeyParser;

impl KeyParser {
    /// Feed one key; returns the outcome. The parser state lives
    /// with the caller so the TUI can display pending state in the
    /// status bar.
    pub fn feed(state: &mut KeyParserState, key: Key) -> KeyOutcome {
        if key == Key::Esc {
            if state.active() {
                state.clear();
            }
            return KeyOutcome::Pending;
        }

        // Leader handling: 'g' waits for a second char.
        if let Some(leader) = state.leader {
            state.leader = None;
            if let Key::Char(c) = key {
                return match (leader, c) {
                    ('g', 'g') => {
                        let target = state.count.take().unwrap_or(1).saturating_sub(1);
                        KeyOutcome::action(Action::GotoPage(target), 1)
                    }
                    _ => {
                        state.count = None;
                        KeyOutcome::Pending
                    }
                };
            } else {
                state.count = None;
                return KeyOutcome::Pending;
            }
        }

        match key {
            Key::Char(c) if c.is_ascii_digit() && (c != '0' || state.count.is_some()) => {
                let d = (c as u8 - b'0') as usize;
                state.count = Some(
                    state
                        .count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(d),
                );
                KeyOutcome::Pending
            }
            Key::Char(':') => {
                state.clear();
                KeyOutcome::OpenCommand
            }
            Key::Char('?') => {
                state.clear();
                KeyOutcome::ToggleHelp
            }
            Key::Char('q') => {
                state.clear();
                KeyOutcome::Quit
            }
            Key::Char('j') | Key::Down => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::NextScreen, n)
            }
            Key::Char('k') | Key::Up => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::PrevScreen, n)
            }
            Key::Char('h') | Key::Left => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::ScrollLeft, n)
            }
            Key::Char('l') | Key::Right => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::ScrollRight, n)
            }
            Key::Char('g') => {
                state.leader = Some('g');
                KeyOutcome::Pending
            }
            Key::Char('G') | Key::End => {
                let outcome = match state.count.take() {
                    Some(n) => KeyOutcome::action(Action::GotoPage(n.saturating_sub(1)), 1),
                    None => KeyOutcome::action(Action::LastPage, 1),
                };
                outcome
            }
            Key::Char('H') => {
                state.count = None;
                KeyOutcome::action(Action::PageTop, 1)
            }
            Key::Char('M') => {
                state.count = None;
                KeyOutcome::action(Action::PageMiddle, 1)
            }
            Key::Char('L') => {
                state.count = None;
                KeyOutcome::action(Action::PageBottom, 1)
            }
            Key::Char('n') => {
                state.count = None;
                KeyOutcome::action(Action::ToggleNight, 1)
            }
            Key::Char('+') | Key::Char('=') => {
                state.count = None;
                KeyOutcome::action(Action::ZoomBy(1.2), 1)
            }
            Key::Char('-') => {
                state.count = None;
                KeyOutcome::action(Action::ZoomBy(1.0 / 1.2), 1)
            }
            Key::Char('w') => {
                state.count = None;
                KeyOutcome::action(Action::SetZoom(ZoomMode::FitWidth), 1)
            }
            Key::Char('e') => {
                state.count = None;
                KeyOutcome::action(Action::SetZoom(ZoomMode::FitHeight), 1)
            }
            Key::Char('f') => {
                state.count = None;
                KeyOutcome::action(Action::SetZoom(ZoomMode::FitPage), 1)
            }
            Key::Char('r') => {
                state.count = None;
                KeyOutcome::action(Action::RotateCw, 1)
            }
            Key::Char('R') => {
                state.count = None;
                KeyOutcome::action(Action::RotateCcw, 1)
            }
            Key::Ctrl('f') | Key::PageDown => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::NextPage, n)
            }
            Key::Ctrl('b') | Key::PageUp => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::PrevPage, n)
            }
            Key::Ctrl('d') => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::HalfScreenDown, n)
            }
            Key::Ctrl('u') => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::action(Action::HalfScreenUp, n)
            }
            Key::Home => {
                state.count = None;
                KeyOutcome::action(Action::FirstPage, 1)
            }
            _ => {
                state.clear();
                KeyOutcome::Pending
            }
        }
    }
}

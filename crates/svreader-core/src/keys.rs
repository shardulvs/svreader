use crate::navigator::Action;
use crate::viewport::ZoomMode;

/// Direction of an arrow key. Used by modified-arrow variants so the
/// parser can branch without eight almost-identical cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowDir {
    Left,
    Down,
    Up,
    Right,
}

/// Page-key direction used by `Ctrl-Shift-Page{Up,Down}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageDir {
    Up,
    Down,
}

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
    /// `Alt-<Arrow>` — vim-users' focus move shortcut.
    AltArrow(ArrowDir),
    /// `Shift-Alt-<Arrow>` — window resize shortcut.
    ShiftAltArrow(ArrowDir),
    /// `Ctrl-PageUp` / `Ctrl-PageDown` — tab switching.
    CtrlPage(PageDir),
    /// `Ctrl-Shift-PageUp` / `Ctrl-Shift-PageDown` — reorder tabs.
    CtrlShiftPage(PageDir),
}

/// Active multi-key chord state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Leader {
    #[default]
    None,
    /// `g` pressed — waiting for second letter (`g`, `t`, `T`, …).
    G,
    /// `Ctrl-w` pressed — waiting for a window command key.
    CtrlW,
    /// `m` pressed — waiting for a mark letter to set.
    M,
    /// `'` pressed — waiting for a mark letter to recall.
    Apostrophe,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyParserState {
    /// Accumulated count prefix (e.g. 42 in `42G`).
    pub count: Option<usize>,
    pub leader: Leader,
}

impl KeyParserState {
    pub fn active(&self) -> bool {
        self.count.is_some() || self.leader != Leader::None
    }

    pub fn clear(&mut self) {
        self.count = None;
        self.leader = Leader::None;
    }

    /// Short hint for the status bar (e.g. `42g` while mid-chord).
    pub fn hint(&self) -> String {
        let mut s = String::new();
        if let Some(c) = self.count {
            s.push_str(&c.to_string());
        }
        match self.leader {
            Leader::None => {}
            Leader::G => s.push('g'),
            Leader::CtrlW => s.push_str("^W"),
            Leader::M => s.push('m'),
            Leader::Apostrophe => s.push('\''),
        }
        s
    }
}

/// Window-manager operations produced by the `Ctrl-w` chord and the
/// tab-switching bindings (`gt` / `gT`, `<C-^>`). Kept as a dedicated
/// enum so `KeyOutcome` stays focused on what kind of event fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOp {
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    /// Cycle focus through windows; `reverse == true` is `<C-w>W`.
    FocusCycle { reverse: bool },
    SplitHorizontal,
    SplitVertical,
    Close,
    Only,
    Equalize,
    /// Positive = grow, negative = shrink, in cell rows.
    ResizeVertical(i32),
    /// Positive = grow, negative = shrink, in cell columns.
    ResizeHorizontal(i32),
    NextTab(usize),
    PrevTab(usize),
    /// Reorder the current tab one position left (`:tabmove -1`).
    MoveTabLeft,
    /// Reorder the current tab one position right (`:tabmove +1`).
    MoveTabRight,
    AlternateBuffer,
}

/// Outcome of feeding a single key into the parser.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// Nothing produced — keep accumulating. Includes "state cleared".
    Pending,
    /// Produced an action, repeated `count` times.
    Action { action: Action, count: usize },
    /// Window / tab manipulation.
    Window(WindowOp),
    /// Enter command-mode (open `:` palette).
    OpenCommand,
    /// Open the search prompt. `forward = true` for `/`, `false` for
    /// `?`.
    OpenSearch { forward: bool },
    /// Step through the active search results. `forward = true` for
    /// `n`, `false` for `N`. The render loop falls back to whatever
    /// `n`/`N` should mean when there's no active search (currently
    /// `n` toggles night, `N` is a no-op).
    SearchStep { forward: bool },
    /// Esc with no active leader / count / mode — cancel transient UI
    /// state. The render loop wires this to "clear search highlights"
    /// (and any future ephemeral overlays).
    Cancel,
    /// Toggle help overlay (`?`).
    ToggleHelp,
    /// Request to quit.
    Quit,
    /// `m{a-z}` — set a single-letter mark on the current viewport.
    SetMark(char),
    /// `'{a-z}` — jump to a previously-set mark.
    JumpMark(char),
    /// `<C-o>` — pop the back-stack.
    JumpBack,
    /// `<C-i>` (rare; usually unreachable from terminals).
    JumpForward,
    /// `t` — toggle the TOC overlay.
    ToggleToc,
}

impl KeyOutcome {
    fn action(action: Action, count: usize) -> Self {
        KeyOutcome::Action {
            action,
            count: count.max(1),
        }
    }

    fn win(op: WindowOp) -> Self {
        KeyOutcome::Window(op)
    }
}

pub struct KeyParser;

impl KeyParser {
    /// Feed one key; returns the outcome. The parser state lives with
    /// the caller so the TUI can display pending state in the status
    /// bar.
    pub fn feed(state: &mut KeyParserState, key: Key) -> KeyOutcome {
        if key == Key::Esc {
            if state.active() {
                state.clear();
                return KeyOutcome::Pending;
            }
            return KeyOutcome::Cancel;
        }

        // Dispatch while in a leader state.
        match state.leader {
            Leader::G => return Self::feed_leader_g(state, key),
            Leader::CtrlW => return Self::feed_leader_ctrl_w(state, key),
            Leader::M => return Self::feed_leader_m(state, key),
            Leader::Apostrophe => return Self::feed_leader_apostrophe(state, key),
            Leader::None => {}
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
                KeyOutcome::OpenSearch { forward: false }
            }
            Key::Char('/') => {
                state.clear();
                KeyOutcome::OpenSearch { forward: true }
            }
            Key::Char('N') => {
                state.clear();
                KeyOutcome::SearchStep { forward: false }
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
                state.leader = Leader::G;
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
                KeyOutcome::SearchStep { forward: true }
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
            Key::Char('t') => {
                state.count = None;
                KeyOutcome::ToggleToc
            }
            Key::Char('m') => {
                state.leader = Leader::M;
                KeyOutcome::Pending
            }
            Key::Char('\'') | Key::Char('`') => {
                state.leader = Leader::Apostrophe;
                KeyOutcome::Pending
            }
            Key::Ctrl('o') => {
                state.count = None;
                KeyOutcome::JumpBack
            }
            Key::Ctrl('i') => {
                state.count = None;
                KeyOutcome::JumpForward
            }
            Key::Ctrl('w') => {
                // Opens the window-manager chord. Count prefix is
                // preserved so `3<C-w>j` moves focus down 3 times.
                state.leader = Leader::CtrlW;
                KeyOutcome::Pending
            }
            Key::Ctrl('^') | Key::Ctrl('6') => {
                state.count = None;
                KeyOutcome::win(WindowOp::AlternateBuffer)
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

            // -- Modifier + arrow / page: direct focus / resize /
            //    tab operations (no `Ctrl-w` prefix needed).
            Key::AltArrow(d) => {
                state.count = None;
                let op = match d {
                    ArrowDir::Left => WindowOp::FocusLeft,
                    ArrowDir::Down => WindowOp::FocusDown,
                    ArrowDir::Up => WindowOp::FocusUp,
                    ArrowDir::Right => WindowOp::FocusRight,
                };
                KeyOutcome::win(op)
            }
            Key::ShiftAltArrow(d) => {
                // Honour count prefix so e.g. `5<S-A-Right>` grows by 5.
                let n = state.count.take().unwrap_or(2).max(1) as i32;
                let op = match d {
                    ArrowDir::Left => WindowOp::ResizeHorizontal(-n),
                    ArrowDir::Right => WindowOp::ResizeHorizontal(n),
                    ArrowDir::Up => WindowOp::ResizeVertical(-n),
                    ArrowDir::Down => WindowOp::ResizeVertical(n),
                };
                KeyOutcome::win(op)
            }
            Key::CtrlPage(d) => {
                state.count = None;
                let op = match d {
                    PageDir::Up => WindowOp::PrevTab(1),
                    PageDir::Down => WindowOp::NextTab(1),
                };
                KeyOutcome::win(op)
            }
            Key::CtrlShiftPage(d) => {
                state.count = None;
                let op = match d {
                    PageDir::Up => WindowOp::MoveTabLeft,
                    PageDir::Down => WindowOp::MoveTabRight,
                };
                KeyOutcome::win(op)
            }
            _ => {
                state.clear();
                KeyOutcome::Pending
            }
        }
    }

    /// After `g` has been pressed. Handles `gg`, `gt`, `gT`.
    fn feed_leader_g(state: &mut KeyParserState, key: Key) -> KeyOutcome {
        state.leader = Leader::None;
        let Key::Char(c) = key else {
            state.count = None;
            return KeyOutcome::Pending;
        };
        match c {
            'g' => {
                let target = state.count.take().unwrap_or(1).saturating_sub(1);
                KeyOutcome::action(Action::GotoPage(target), 1)
            }
            't' => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::win(WindowOp::NextTab(n))
            }
            'T' => {
                let n = state.count.take().unwrap_or(1);
                KeyOutcome::win(WindowOp::PrevTab(n))
            }
            _ => {
                state.count = None;
                KeyOutcome::Pending
            }
        }
    }

    /// After `m` has been pressed. Consumes one letter `[a-zA-Z]` and
    /// emits `SetMark`. Anything else aborts the chord cleanly.
    fn feed_leader_m(state: &mut KeyParserState, key: Key) -> KeyOutcome {
        state.leader = Leader::None;
        state.count = None;
        let Key::Char(c) = key else {
            return KeyOutcome::Pending;
        };
        if c.is_ascii_alphabetic() {
            KeyOutcome::SetMark(c)
        } else {
            KeyOutcome::Pending
        }
    }

    /// After `'` (or backtick) has been pressed. Same semantics as
    /// `m`, but jumps instead of sets.
    fn feed_leader_apostrophe(state: &mut KeyParserState, key: Key) -> KeyOutcome {
        state.leader = Leader::None;
        state.count = None;
        let Key::Char(c) = key else {
            return KeyOutcome::Pending;
        };
        if c.is_ascii_alphabetic() {
            KeyOutcome::JumpMark(c)
        } else {
            KeyOutcome::Pending
        }
    }

    /// After `Ctrl-w` has been pressed. Handles the vim window chord.
    fn feed_leader_ctrl_w(state: &mut KeyParserState, key: Key) -> KeyOutcome {
        state.leader = Leader::None;
        let n = state.count.take().unwrap_or(1).max(1) as i32;
        // Accept both plain chars and Ctrl-char forms (`<C-w><C-l>`
        // is a common vim habit — same effect as `<C-w>l`).
        let c = match key {
            Key::Char(c) => c,
            Key::Ctrl(c) => c,
            _ => return KeyOutcome::Pending,
        };
        let op = match c {
            'h' => WindowOp::FocusLeft,
            'j' => WindowOp::FocusDown,
            'k' => WindowOp::FocusUp,
            'l' => WindowOp::FocusRight,
            'w' => WindowOp::FocusCycle { reverse: false },
            'W' => WindowOp::FocusCycle { reverse: true },
            's' => WindowOp::SplitHorizontal,
            'v' => WindowOp::SplitVertical,
            'c' => WindowOp::Close,
            'o' => WindowOp::Only,
            '=' => WindowOp::Equalize,
            '+' => WindowOp::ResizeVertical(n),
            '-' => WindowOp::ResizeVertical(-n),
            '>' => WindowOp::ResizeHorizontal(n),
            '<' => WindowOp::ResizeHorizontal(-n),
            _ => return KeyOutcome::Pending,
        };
        KeyOutcome::win(op)
    }
}

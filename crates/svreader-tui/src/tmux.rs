//! tmux passthrough support.
//!
//! tmux intercepts DCS sequences (including sixel) from programs
//! running inside it. To forward a DCS to the outer terminal, wrap it
//! in a tmux passthrough envelope: `ESC P tmux ; <payload> ESC \` with
//! every inner `ESC` doubled (tmux strips one on the way out).
//!
//! Requires `set -g allow-passthrough on` in tmux ≥ 3.3.

use std::env;

/// True if we're running inside a tmux session.
pub fn in_tmux() -> bool {
    env::var_os("TMUX").is_some()
}

/// Wrap an escape-sequence payload in a tmux passthrough envelope.
/// Safe to call even outside tmux; returns `payload` unchanged then.
pub fn wrap_for_tmux(payload: &str) -> String {
    if !in_tmux() {
        return payload.to_string();
    }
    let mut out = String::with_capacity(payload.len() + 16);
    out.push_str("\x1bPtmux;");
    for c in payload.chars() {
        if c == '\x1b' {
            out.push('\x1b');
            out.push('\x1b');
        } else {
            out.push(c);
        }
    }
    out.push_str("\x1b\\");
    out
}

//! Sixel capability probe via `CSI c` (Device Attributes).

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::tmux::wrap_for_tmux;

/// Probe sixel support. Returns true if the terminal advertises
/// "4" in its primary device attributes response. We fail closed
/// (return `false`) on a timeout/parse failure so callers can print
/// a clear error.
pub fn probe_sixel(timeout: Duration) -> bool {
    match probe_impl(timeout) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!("sixel probe failed: {e:#}");
            false
        }
    }
}

fn probe_impl(timeout: Duration) -> Result<bool> {
    let mut out = io::stdout();
    let query = wrap_for_tmux("\x1b[c");
    out.write_all(query.as_bytes())?;
    out.flush()?;

    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    let start = Instant::now();
    let mut buf = Vec::with_capacity(64);
    let mut tmp = [0u8; 64];
    while start.elapsed() < timeout {
        let remaining = timeout - start.elapsed();
        if !fd_readable(fd, remaining)? {
            break;
        }
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        if buf.contains(&b'c') {
            break;
        }
    }
    let s = std::str::from_utf8(&buf).map_err(|_| anyhow!("non-utf8 DA1 reply"))?;
    // Response is ESC [ ? <n>;<n>;... c
    // Sixel-capable terminals include "4" in the semicolon list.
    let has_sixel = s
        .split_terminator('c')
        .next()
        .unwrap_or("")
        .split(';')
        .any(|p| p.trim().trim_start_matches(|c: char| !c.is_ascii_digit()) == "4");
    Ok(has_sixel)
}

#[cfg(unix)]
fn fd_readable(fd: i32, timeout: Duration) -> Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
    if rc < 0 {
        return Err(anyhow!("poll failed: {}", io::Error::last_os_error()));
    }
    Ok(rc > 0 && (pfd.revents & libc::POLLIN) != 0)
}

#[cfg(not(unix))]
fn fd_readable(_fd: i32, _timeout: Duration) -> Result<bool> {
    Ok(false)
}

pub const SIXEL_TERMINALS: &str =
    "WezTerm, foot, Ghostty, Konsole, xterm (-ti vt340), iTerm2, mintty, Windows Terminal, mlterm";

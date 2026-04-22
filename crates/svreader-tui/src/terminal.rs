//! Terminal pixel geometry and capability discovery.
//!
//! We try, in order:
//!  1. `$SVREADER_SCREEN_PX=WxH` env override (debug only).
//!  2. `ioctl(TIOCGWINSZ)` for pixel dims.
//!  3. CSI 14 t / CSI 16 t queries (tmux often reports zero pixels via
//!     the ioctl).

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::tmux::wrap_for_tmux;

#[derive(Debug, Clone, Copy)]
pub struct TermGeom {
    pub cols: u16,
    pub rows: u16,
    pub px_w: u16,
    pub px_h: u16,
    pub cell_px_w: u16,
    pub cell_px_h: u16,
}


pub fn query(override_env: Option<&str>) -> Result<TermGeom> {
    let (cols, rows, px_w, px_h) = read_winsize().unwrap_or((80, 24, 0, 0));

    if let Some(ov) = override_env {
        if let Some((w, h)) = parse_wxh(ov) {
            return Ok(compose(cols, rows, w, h));
        }
    }

    let (mut pw, mut ph) = (px_w as u32, px_h as u32);

    if pw == 0 || ph == 0 {
        // tmux adds latency; give the roundtrip more headroom.
        let timeout = if crate::tmux::in_tmux() {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(250)
        };
        if let Ok((w, h)) = query_csi_14t(timeout) {
            pw = w;
            ph = h;
        }
    }

    if pw == 0 || ph == 0 {
        // Reasonable fallback guess: 8x17 cell, typical for many
        // terminals. Not great, but better than crashing.
        pw = cols as u32 * 8;
        ph = rows as u32 * 17;
    }

    Ok(compose(cols, rows, pw, ph))
}

fn compose(cols: u16, rows: u16, pw: u32, ph: u32) -> TermGeom {
    let pw = pw.max(1);
    let ph = ph.max(1);
    let cell_w = (pw / cols.max(1) as u32).max(1) as u16;
    let cell_h = (ph / rows.max(1) as u32).max(1) as u16;
    TermGeom {
        cols,
        rows,
        px_w: pw.min(u16::MAX as u32) as u16,
        px_h: ph.min(u16::MAX as u32) as u16,
        cell_px_w: cell_w,
        cell_px_h: cell_h,
    }
}

fn parse_wxh(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once('x')?;
    let w = a.parse().ok()?;
    let h = b.parse().ok()?;
    Some((w, h))
}

#[cfg(unix)]
fn read_winsize() -> Option<(u16, u16, u16, u16)> {
    use libc::{ioctl, winsize, TIOCGWINSZ};
    let mut ws: winsize = unsafe { std::mem::zeroed() };
    let stdout = io::stdout();
    let fd = stdout.as_raw_fd();
    let rc = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws) };
    if rc == -1 {
        return None;
    }
    Some((ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel))
}

#[cfg(not(unix))]
fn read_winsize() -> Option<(u16, u16, u16, u16)> {
    None
}

/// Query `CSI 14 t` → response `ESC [ 4 ; H ; W t`. Under tmux we
/// wrap the query in a passthrough envelope so the outer terminal
/// answers us directly.
fn query_csi_14t(timeout: Duration) -> Result<(u32, u32)> {
    let query = wrap_for_tmux("\x1b[14t");
    let resp = send_query(query.as_bytes(), timeout)?;
    let s = std::str::from_utf8(&resp).map_err(|_| anyhow!("non-utf8 CSI response"))?;
    // Find the ESC [ 4 ; <H> ; <W> t substring — other terminal
    // chatter (focus events, etc.) can land alongside.
    let body = match extract_csi_body(s, 't') {
        Some(b) => b,
        None => return Err(anyhow!("no CSI 14t reply in {s:?}")),
    };
    let parts: Vec<_> = body.split(';').collect();
    if parts.len() != 3 || parts[0] != "4" {
        return Err(anyhow!("unexpected CSI 14 t reply: {s:?}"));
    }
    let h: u32 = parts[1].parse()?;
    let w: u32 = parts[2].parse()?;
    Ok((w, h))
}

/// Find the first `ESC [ ... <end>` body in a buffer and return the
/// bytes between `[` and `<end>`. Returns None if not found.
pub(crate) fn extract_csi_body(s: &str, end: char) -> Option<&str> {
    let start = s.find("\x1b[")?;
    let rest = &s[start + 2..];
    let end_idx = rest.find(end)?;
    Some(&rest[..end_idx])
}

/// Send a query to stdout and read the response from stdin with a
/// timeout. Requires raw mode to be active.
fn send_query(query: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let mut out = io::stdout();
    out.write_all(query)?;
    out.flush()?;

    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();

    // Use poll() for fd readability with timeout.
    let start = Instant::now();
    let mut buf = Vec::with_capacity(64);
    let mut tmp = [0u8; 64];
    while start.elapsed() < timeout {
        let remaining = timeout - start.elapsed();
        if !fd_readable(fd, remaining)? {
            break;
        }
        // Safety: we use libc::read directly to avoid blocking behaviour.
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        if buf.ends_with(b"t") || buf.ends_with(b"c") || buf.contains(&b't') {
            break;
        }
    }
    if buf.is_empty() {
        return Err(anyhow!("no response to terminal query"));
    }
    Ok(buf)
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

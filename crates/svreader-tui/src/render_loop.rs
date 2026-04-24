//! The main event loop. Owns terminal I/O; delegates state to
//! `Workspace`.
//!
//! Render model:
//!   row 0              tab bar (only when >1 tab)
//!   rows 1..rows-2     tab body: the current tab's split tree
//!   row rows-1         global status bar (focused window's info)
//!
//! Each window inside the body gets the full cell rectangle for its
//! sixel image. Unfocused windows are repainted when focus toggles so
//! their outline reflects the change; they don't own a separate
//! title-row in M1.5a.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use svreader_core::cache::{CacheKey, CachedPage};
use svreader_core::keys::{ArrowDir, Key, KeyOutcome, KeyParser, KeyParserState, PageDir};
use svreader_core::{
    Action, ColorPalette, CommandRegistry, Document, PageCache, ParsedCommand, PrefetchRequest,
    Renderer, Rotation, Viewport, ZoomMode,
};

use crate::capabilities::{probe_sixel, SIXEL_TERMINALS};
use crate::sixel_write::{blank_rect, encode_and_write, ColorMode};
use crate::terminal::{self, TermGeom};
use crate::timings::{FrameTiming, TimingsLog};
use crate::window::{CellRect, WindowId};
use crate::workspace::Workspace;
use crate::RunOptions;

const STATUS_ROWS: u16 = 1;
const PALETTE_MAX_ROWS: u16 = 6;
const HELP_ROWS: u16 = 20;

#[derive(Debug, Clone)]
enum Mode {
    Normal,
    Command {
        input: String,
        completion_idx: Option<usize>,
    },
    Help,
}

pub fn run(opts: RunOptions) -> Result<()> {
    let pdf_path = opts.pdf.clone();

    enable_raw_mode().context("enable_raw_mode failed")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)
        .context("alt-screen enter failed")?;

    let res = run_inner(opts, pdf_path, &mut stdout);

    let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
    res
}

struct AppState {
    key_state: KeyParserState,
    mode: Mode,
    pending_hint: String,
    message: Option<String>,
    message_expires: Option<Instant>,
    /// Dirty flag for the tab bar / global status bar.
    chrome_dirty: bool,
    /// True until the next key/resize — triggers a full clear.
    full_repaint: bool,
}

fn run_inner(
    opts: RunOptions,
    pdf_path: PathBuf,
    stdout: &mut io::Stdout,
) -> Result<()> {
    // Loading banner before anything slow.
    write!(stdout, "\x1b[2J\x1b[H")?;
    write!(stdout, "svreader — loading {}...\r\n", pdf_path.display())?;
    if crate::tmux::in_tmux() {
        write!(
            stdout,
            "  (tmux detected: requires `set -g allow-passthrough on` in ~/.tmux.conf)\r\n"
        )?;
    }
    stdout.flush()?;

    let mut geom = terminal::query(opts.screen_px_override.as_deref())?;
    let probe_timeout = if crate::tmux::in_tmux() {
        Duration::from_millis(600)
    } else {
        Duration::from_millis(250)
    };
    if !probe_sixel(probe_timeout) {
        tracing::warn!(
            "terminal did not advertise sixel (CSI c). If you see no image, try one of: {SIXEL_TERMINALS}"
        );
    }

    let cache = Arc::new(PageCache::new(5));
    let initial_body = body_rect(geom, 0); // tab bar not yet rendered
    let (img_w, img_h) = pixel_size(initial_body, geom);

    // Seed the viewport from the sidecar so `last_page`, zoom, etc.
    // survive across restarts. Workspace::with_pdf also seeds from
    // DocState, so this mostly fills in the screen dims.
    let initial_viewport = Viewport {
        screen_w: img_w.max(1),
        screen_h: img_h.max(1),
        ..Default::default()
    };
    let mut ws = Workspace::with_pdf(cache.clone(), &pdf_path, initial_viewport)?;

    // Apply the start-page override (after DocState load).
    if let Some(page) = opts.start_page {
        let idx = page.saturating_sub(1);
        let buf_id = ws.focused_window().buffer;
        if let Some(buf) = ws.buffer_mut(buf_id) {
            let n = buf.pdf.page_count();
            if n > 0 {
                ws.focused_window_mut().viewport.page_idx = idx.min(n - 1);
            }
        }
    }
    ws.propagate_geometry(geom.cell_px_w, geom.cell_px_h, initial_body);
    // Remember layout geometry for focus_neighbour etc.
    let _ = ws.layout(initial_body);

    let timings_log = {
        let log_path = std::env::var("SVREADER_TIMINGS_LOG")
            .ok()
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from("/tmp/svreader-timings.log")));
        TimingsLog::open(log_path)
    };
    let registry = CommandRegistry::default();

    let mut app = AppState {
        key_state: KeyParserState::default(),
        mode: Mode::Normal,
        pending_hint: String::new(),
        message: None,
        message_expires: None,
        chrome_dirty: true,
        full_repaint: true,
    };

    while !ws.quit_requested {
        // Expire messages.
        if let Some(t) = app.message_expires {
            if Instant::now() >= t {
                app.message = None;
                app.message_expires = None;
                app.chrome_dirty = true;
            }
        }
        // Pull message from workspace if there is one.
        if let Some(msg) = ws.message.take() {
            if msg.as_str() != "__workspace_passthrough__" {
                set_message(&mut app, msg);
            }
        }

        if app.full_repaint {
            write!(stdout, "\x1b[2J")?;
            app.full_repaint = false;
            app.chrome_dirty = true;
            for w in ws.current_tab_mut().tree.windows_mut() {
                w.dirty = true;
                w.last_rect = None;
                w.last_sixel_rows = 0;
            }
        }

        let tab_bar_rows: u16 = if ws.tab_count() > 1 { 1 } else { 0 };
        let body = body_rect(geom, tab_bar_rows);
        let layout = ws.layout(body);

        // Tab bar and status bar always redraw — both are a single row
        // of text each, and tying them to a dirty flag was getting us
        // into trouble (e.g. `<C-PageDown>` would switch tabs but the
        // tab bar wouldn't reflect the new current tab until the next
        // `chrome_dirty` event).
        draw_tab_bar(stdout, &ws, geom)?;

        // Paint windows.
        paint_windows(stdout, &mut ws, &cache, &timings_log, &layout, geom)?;

        draw_status(stdout, &ws, &app, geom)?;
        app.chrome_dirty = false;

        // Overlays.
        match &app.mode {
            Mode::Normal => {}
            Mode::Command {
                input,
                completion_idx,
            } => {
                let completions = compute_completions(input, &registry);
                let display: Vec<String> =
                    completions.iter().map(|c| c.display.clone()).collect();
                let bottom = geom.rows.saturating_sub(STATUS_ROWS);
                let top = bottom.saturating_sub(PALETTE_MAX_ROWS);
                draw_palette(stdout, top, bottom, geom.cols, input, &display, *completion_idx)?;
                let input_row = bottom.saturating_sub(1);
                let col = (input.chars().count() as u16).saturating_add(2);
                write!(stdout, "\x1b[{};{}H", input_row + 1, col)?;
                execute!(stdout, cursor::Show)?;
            }
            Mode::Help => {
                let bottom = geom.rows.saturating_sub(STATUS_ROWS);
                let top = bottom.saturating_sub(HELP_ROWS);
                draw_help(stdout, top, bottom, geom.cols)?;
            }
        }
        stdout.flush()?;

        // Fire prefetches around the focused window's page.
        fire_prefetches(&mut ws);

        let poll_timeout = if app.message_expires.is_some() {
            Duration::from_millis(200)
        } else {
            Duration::from_millis(1000)
        };
        if !event::poll(poll_timeout)? {
            continue;
        }
        match event::read()? {
            Event::Resize(cols, rows) => {
                let new_geom = terminal::query(opts.screen_px_override.as_deref())
                    .unwrap_or(TermGeom {
                        cols,
                        rows,
                        px_w: geom.px_w,
                        px_h: geom.px_h,
                        cell_px_w: geom.cell_px_w,
                        cell_px_h: geom.cell_px_h,
                    });
                geom = new_geom;
                let tab_bar_rows: u16 = if ws.tab_count() > 1 { 1 } else { 0 };
                let body = body_rect(geom, tab_bar_rows);
                ws.propagate_geometry(geom.cell_px_w, geom.cell_px_h, body);
                cache.clear();
                app.full_repaint = true;
            }
            Event::Key(k) => {
                if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                execute!(stdout, cursor::Hide)?;
                match app.mode.clone() {
                    Mode::Normal => handle_normal_key(&mut ws, &mut app, k)?,
                    Mode::Command { .. } => {
                        handle_command_key(&mut ws, &cache, &registry, &mut app, k, stdout)?;
                    }
                    Mode::Help => {
                        if matches!(k.code, KeyCode::Esc)
                            || k.code == KeyCode::Char('?')
                            || k.code == KeyCode::Char('q')
                        {
                            app.mode = Mode::Normal;
                            app.full_repaint = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    ws.persist_all();
    Ok(())
}

/// Reserve the top `tab_bar_rows` and bottom `STATUS_ROWS` rows and
/// return the cell rect for the window body.
fn body_rect(geom: TermGeom, tab_bar_rows: u16) -> CellRect {
    let row = tab_bar_rows;
    let rows = geom
        .rows
        .saturating_sub(tab_bar_rows)
        .saturating_sub(STATUS_ROWS)
        .max(1);
    CellRect {
        col: 0,
        row,
        cols: geom.cols.max(1),
        rows,
    }
}

fn pixel_size(rect: CellRect, geom: TermGeom) -> (u32, u32) {
    let w = (rect.cols as u32) * (geom.cell_px_w as u32);
    let h = (rect.rows as u32) * (geom.cell_px_h as u32);
    (w.max(1), h.max(1))
}

fn paint_windows(
    stdout: &mut impl Write,
    ws: &mut Workspace,
    cache: &Arc<PageCache>,
    timings_log: &TimingsLog,
    layout: &[(WindowId, CellRect)],
    geom: TermGeom,
) -> Result<()> {
    // Flatten layout so we can mutate windows without fighting the
    // tree borrow.
    let layout_map: Vec<(WindowId, CellRect)> = layout.iter().copied().collect();
    for (id, rect) in layout_map {
        let dirty = ws
            .current_tab()
            .tree
            .find(id)
            .map(|w| w.dirty || w.last_rect != Some(rect))
            .unwrap_or(false);
        if !dirty {
            continue;
        }
        paint_window(stdout, ws, cache, timings_log, id, rect, geom)?;
    }
    Ok(())
}

fn paint_window(
    stdout: &mut impl Write,
    ws: &mut Workspace,
    cache: &Arc<PageCache>,
    timings_log: &TimingsLog,
    id: WindowId,
    rect: CellRect,
    geom: TermGeom,
) -> Result<()> {
    let image_rect = rect;
    let (img_w, img_h) = pixel_size(image_rect, geom);

    // Blank the previous rect if it moved or shrank so we don't
    // leave sixel pixels outside the new image area.
    let buffer_id = {
        let w = ws
            .current_tab_mut()
            .tree
            .find_mut(id)
            .expect("window in layout");
        if let Some(prev) = w.last_rect {
            if prev != rect {
                blank_rect(prev.col, prev.row, prev.cols, prev.rows, stdout).ok();
            }
        }
        w.buffer
    };

    // Re-snap offsets (fit-width, centring, etc.) when the window
    // resized. Goes through Navigator so zoom/rotation anchors stay
    // consistent with the rest of the navigator state machine.
    let _ = ws.resync_window_viewport(id, img_w, img_h)?;

    let Some(buf) = ws.buffer(buffer_id) else {
        return Ok(());
    };

    // Compose viewport + cache key. Borrowed snapshot.
    let (display_scale, raster_scale, viewport, rotation, page_idx) = {
        let w = ws.current_tab().tree.find(id).unwrap();
        let page_size = buf.pdf.page_size(w.viewport.page_idx)?;
        let display_scale = w.viewport.display_scale(page_size);
        let raster_scale = w.viewport.raster_scale(page_size);
        (
            display_scale,
            raster_scale,
            w.viewport.clone(),
            w.viewport.rotation,
            w.viewport.page_idx,
        )
    };
    let key = CacheKey::new(buffer_id, page_idx, display_scale, raster_scale, rotation);

    let t_overall = Instant::now();
    let (page, render_dur) = if let Some(hit) = cache.get(&key) {
        (hit, Duration::ZERO)
    } else {
        let (page, rt) = Renderer::render_page(&buf.pdf, &viewport)?;
        let arc: Arc<CachedPage> = Arc::new(page);
        cache.insert(key, arc.clone());
        (arc, rt.render)
    };
    let (composed, compose) = Renderer::compose(&page, &viewport);

    let color_mode = ws
        .current_tab()
        .tree
        .find(id)
        .map(|w| w.color_mode)
        .unwrap_or(ColorMode::XTerm256);

    let emit = encode_and_write(&composed, image_rect.col, image_rect.row, color_mode, stdout)?;

    let total = t_overall.elapsed();
    let attributed = render_dur + compose.compose + emit.encode + emit.write;
    let other = total.saturating_sub(attributed);
    let timing = FrameTiming {
        render: render_dur,
        compose: compose.compose,
        encode: emit.encode,
        write: emit.write,
        other,
    };
    timings_log.record(page_idx, &timing);

    // Write back per-window stats.
    let effective_dpi = {
        let page_size = buf.pdf.page_size(page_idx)?;
        viewport.effective_dpi(page_size)
    };
    let w = ws.current_tab_mut().tree.find_mut(id).unwrap();
    w.last_timing = Some(timing);
    w.last_dpi = effective_dpi;
    w.last_sixel_rows = (composed.height() as u32).div_ceil(geom.cell_px_h as u32) as u16;
    w.last_rect = Some(rect);
    w.dirty = false;
    Ok(())
}

fn draw_tab_bar(stdout: &mut impl Write, ws: &Workspace, geom: TermGeom) -> Result<()> {
    if ws.tab_count() <= 1 {
        return Ok(());
    }
    write!(stdout, "\x1b[1;1H\x1b[2K")?;
    let mut line = String::new();
    for i in 0..ws.tab_count() {
        let tab_name = tab_title(ws, i);
        let prefix = if i == ws.current_tab_index() {
            "\x1b[48;5;238m\x1b[38;5;252m "
        } else {
            "\x1b[48;5;236m\x1b[38;5;244m "
        };
        line.push_str(prefix);
        line.push_str(&format!("{} ", tab_name));
        line.push_str("\x1b[0m");
    }
    // Truncate to cols.
    let truncated: String = line.chars().take(geom.cols as usize * 16).collect();
    write!(stdout, "{}", truncated)?;
    Ok(())
}

fn tab_title(ws: &Workspace, tab_idx: usize) -> String {
    let Some(tab) = ws.tab(tab_idx) else {
        return format!("tab {}", tab_idx + 1);
    };
    let focused = tab.tree.find(tab.focused);
    if let Some(id) = focused.map(|w| w.buffer) {
        if let Some(buf) = ws.buffer(id) {
            return buf.display_name();
        }
    }
    format!("tab {}", tab_idx + 1)
}

fn draw_status(
    stdout: &mut impl Write,
    ws: &Workspace,
    app: &AppState,
    geom: TermGeom,
) -> Result<()> {
    let row = geom.rows.saturating_sub(STATUS_ROWS);
    let focused = ws.focused_window();
    let buf = ws.buffer(focused.buffer);
    let file_name = buf
        .map(|b| b.display_name())
        .unwrap_or_else(|| "document".into());
    let page_count = buf.map(|b| b.pdf.page_count()).unwrap_or(1);
    let cache_stats = ws.cache.stats();

    let mut s = String::new();
    s.push_str(&format!(
        " {} | {}/{} | {} | {}\u{00B0}",
        file_name,
        focused.viewport.page_idx + 1,
        page_count.max(1),
        focused.viewport.zoom.label(),
        focused.viewport.rotation.degrees(),
    ));
    if focused.viewport.night_mode {
        s.push_str(" [night]");
    }
    s.push_str(&format!(
        " dpi:{}{}",
        focused.last_dpi.round() as i32,
        if focused.viewport.render_dpi.is_some() { "*" } else { "" }
    ));
    if (focused.viewport.render_quality - 1.0).abs() > 0.005 {
        s.push_str(&format!(
            " q:{}%",
            (focused.viewport.render_quality * 100.0).round() as i32
        ));
    }
    s.push_str(&format!(" cache:{}/{}", cache_stats.0, cache_stats.1));
    if let Some(t) = &focused.last_timing {
        s.push(' ');
        s.push_str(&t.short_label());
    }
    if ws.current_tab().tree.leaf_count() > 1 {
        s.push_str(&format!(" w:{}", ws.current_tab().tree.leaf_count()));
    }
    if ws.tab_count() > 1 {
        s.push_str(&format!(
            " t:{}/{}",
            ws.current_tab_index() + 1,
            ws.tab_count()
        ));
    }
    if !app.pending_hint.is_empty() {
        s.push_str(&format!(" [{}]", app.pending_hint));
    }
    if let Some(msg) = &app.message {
        s.push_str(&format!(" -- {}", msg));
    }
    let truncated: String = s.chars().take(geom.cols as usize).collect();
    let pad = (geom.cols as usize).saturating_sub(truncated.chars().count());
    write!(
        stdout,
        "\x1b[{};1H\x1b[2K\x1b[48;5;236m\x1b[38;5;252m{}{}\x1b[0m",
        row + 1,
        truncated,
        " ".repeat(pad)
    )?;
    Ok(())
}

fn draw_palette(
    stdout: &mut impl Write,
    top: u16,
    bottom: u16,
    cols: u16,
    input: &str,
    completions: &[String],
    cursor_idx: Option<usize>,
) -> Result<()> {
    // Blank the whole palette area first.
    for r in top..bottom {
        write!(stdout, "\x1b[{};1H\x1b[2K", r + 1)?;
    }

    // Layout: input row at the bottom, completions above it with
    // index 0 at the top and index N-1 directly above the input. So
    // `Down` (which increments the index) visually moves the
    // highlight toward the input, matching user intuition.
    let max_comp = (bottom - top).saturating_sub(1) as usize;
    let total = completions.len();
    let visible = max_comp.min(total);

    // Scroll the window so the selected entry is always inside it.
    // Keeps selection at the bottom of the window once the user
    // cycles past the last visible row, mirroring readline's menu.
    let scroll = match cursor_idx {
        Some(idx) if idx >= visible => idx + 1 - visible,
        _ => 0,
    };

    let input_row = bottom.saturating_sub(1);
    let start_row = input_row.saturating_sub(visible as u16);

    for local_i in 0..visible {
        let abs_i = scroll + local_i;
        let Some(c) = completions.get(abs_i) else {
            break;
        };
        let row = start_row + local_i as u16;
        write!(stdout, "\x1b[{};1H", row + 1)?;
        let selected = Some(abs_i) == cursor_idx;
        if selected {
            write!(stdout, "\x1b[7m")?;
        }
        let line: String = c.chars().take(cols as usize).collect();
        let pad = (cols as usize).saturating_sub(line.chars().count());
        write!(stdout, "{}{}", line, " ".repeat(pad))?;
        if selected {
            write!(stdout, "\x1b[0m")?;
        }
    }

    // Input line at the bottom.
    write!(stdout, "\x1b[{};1H", input_row + 1)?;
    let line = format!(":{}", input);
    let truncated: String = line.chars().take(cols as usize).collect();
    let pad = (cols as usize).saturating_sub(truncated.chars().count());
    write!(stdout, "{}{}", truncated, " ".repeat(pad))?;
    Ok(())
}

fn draw_help(stdout: &mut impl Write, top: u16, bottom: u16, cols: u16) -> Result<()> {
    let lines: &[&str] = &[
        " svreader — keys",
        "",
        "   j / k         next / prev screen (10% overlap)",
        "   Ctrl-d/u      half-screen down/up",
        "   Ctrl-f/b      next/prev page (no overlap)",
        "   gg / G        first / last page",
        "   H M L         page top / middle / bottom",
        "   h / l         scroll left / right",
        "   w / e / f     fit width / height / page",
        "   + / -         zoom in / out",
        "   r / R         rotate CW / CCW",
        "   n             toggle night mode",
        "   Ctrl-w h/j/k/l   move focus",
        "   Ctrl-w s / v     split horizontal / vertical",
        "   Ctrl-w c / o     close / only",
        "   gt / gT       next / previous tab",
        "   Ctrl-^        alternate buffer",
        "   :             command palette  (:open, :edit, :split, :tabnew, :q, :qa)",
        "   ?             toggle this help   q   quit",
        "",
        " Press ? or Esc to close.",
    ];
    for (i, r) in (top..bottom).enumerate() {
        write!(stdout, "\x1b[{};1H\x1b[2K", r + 1)?;
        if let Some(text) = lines.get(i) {
            let t: String = text.chars().take(cols as usize).collect();
            write!(stdout, "{}", t)?;
        }
    }
    Ok(())
}

fn fire_prefetches(ws: &mut Workspace) {
    if !ws.cache.enabled() {
        return;
    }
    let focused_buffer = ws.focused_window().buffer;
    let Some(buf) = ws.buffer(focused_buffer) else {
        return;
    };
    let count = buf.pdf.page_count();
    if count == 0 {
        return;
    }
    let focused_page = ws.focused_window().viewport.page_idx;
    let vp_template = ws.focused_window().viewport.clone();
    let n = 2usize;
    let start = focused_page.saturating_sub(n);
    let end = (focused_page + n).min(count.saturating_sub(1));
    for idx in start..=end {
        if idx == focused_page {
            continue;
        }
        let mut vp = vp_template.clone();
        vp.page_idx = idx;
        vp.x_off = 0;
        vp.y_off = 0;
        let ps = match buf.pdf.page_size(idx) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let key = CacheKey::new(
            focused_buffer,
            idx,
            vp.display_scale(ps),
            vp.raster_scale(ps),
            vp.rotation,
        );
        if ws.cache.contains(&key) {
            continue;
        }
        buf.prefetcher
            .request(PrefetchRequest { viewport: vp, key });
    }
}

fn handle_normal_key(ws: &mut Workspace, app: &mut AppState, k: KeyEvent) -> Result<()> {
    if matches!(k.code, KeyCode::Char('c')) && k.modifiers.contains(KeyModifiers::CONTROL) {
        ws.quit_requested = true;
        return Ok(());
    }
    let Some(key) = crossterm_to_key(k) else {
        return Ok(());
    };
    let outcome = KeyParser::feed(&mut app.key_state, key);
    app.pending_hint = app.key_state.hint();
    match outcome {
        KeyOutcome::Pending => {
            app.chrome_dirty = true;
        }
        KeyOutcome::OpenCommand => {
            app.mode = Mode::Command {
                input: String::new(),
                completion_idx: None,
            };
        }
        KeyOutcome::ToggleHelp => {
            app.mode = Mode::Help;
        }
        KeyOutcome::Quit => {
            // 'q' closes focused window (vim-like); quits if last.
            ws.apply_command(ParsedCommand::CloseWindow)?;
        }
        KeyOutcome::Action { action, count } => {
            ws.apply_nav(action, count)?;
        }
        KeyOutcome::Window(op) => {
            if let Err(e) = ws.apply_window_op(op) {
                set_message(app, format!("{e}"));
            }
        }
    }
    Ok(())
}

fn handle_command_key(
    ws: &mut Workspace,
    cache: &Arc<PageCache>,
    registry: &CommandRegistry,
    app: &mut AppState,
    k: KeyEvent,
    stdout: &mut impl Write,
) -> Result<()> {
    let Mode::Command {
        input,
        completion_idx,
    } = &mut app.mode
    else {
        return Ok(());
    };
    match k.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.full_repaint = true;
        }
        KeyCode::Enter => {
            // Two-stage Enter when a completion is highlighted: the
            // first Enter pastes the selection into the input and
            // stays in the palette; the second (with nothing
            // highlighted) executes. Lets the user review a
            // suggested path before committing.
            if let Some(idx) = *completion_idx {
                let completions = compute_completions(input, registry);
                if let Some(entry) = completions.get(idx) {
                    input.truncate(entry.replace_from);
                    input.push_str(&entry.insert);
                }
                *completion_idx = None;
                return Ok(());
            }
            let line = std::mem::take(input);
            app.mode = Mode::Normal;
            app.full_repaint = true;
            if !line.is_empty() {
                if let Err(e) = execute_command(ws, cache, app, registry, &line) {
                    set_message(app, format!("{e}"));
                }
            }
        }
        KeyCode::Tab | KeyCode::Down => {
            cycle_completion(input, completion_idx, registry, false);
        }
        KeyCode::BackTab | KeyCode::Up => {
            cycle_completion(input, completion_idx, registry, true);
        }
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            cycle_completion(input, completion_idx, registry, false);
        }
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            cycle_completion(input, completion_idx, registry, true);
        }
        KeyCode::Backspace => {
            if !input.is_empty() {
                input.pop();
                *completion_idx = None;
            }
        }
        KeyCode::Char(c) => {
            if k.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                app.mode = Mode::Normal;
                app.full_repaint = true;
            } else {
                input.push(c);
                *completion_idx = None;
            }
        }
        _ => {}
    }
    let _ = stdout;
    Ok(())
}

fn execute_command(
    ws: &mut Workspace,
    cache: &Arc<PageCache>,
    app: &mut AppState,
    registry: &CommandRegistry,
    line: &str,
) -> Result<()> {
    let parsed = registry.parse(line)?;
    match parsed {
        ParsedCommand::Help => {
            app.mode = Mode::Help;
        }
        ParsedCommand::CacheSet(b) => {
            cache.set_enabled(b);
            set_message(app, format!("cache {}", if b { "on" } else { "off" }));
        }
        ParsedCommand::CacheToggle => {
            cache.set_enabled(!cache.enabled());
            set_message(
                app,
                format!("cache {}", if cache.enabled() { "on" } else { "off" }),
            );
        }
        ParsedCommand::CacheSize(n) => {
            cache.resize(n);
            set_message(app, format!("cache-size {}", n));
        }
        ParsedCommand::Prefetch(n) => {
            set_message(app, format!("prefetch {}", n));
            let _ = n;
        }
        ParsedCommand::Reset => {
            let w = ws.focused_window_mut();
            w.viewport.render_dpi = None;
            w.viewport.render_quality = 1.0;
            w.dirty = true;
            ws.apply_nav(Action::SetRotation(Rotation::R0), 1)?;
            ws.apply_nav(Action::SetZoom(ZoomMode::FitWidth), 1)?;
            cache.clear();
        }
        ParsedCommand::Colors(p) => {
            let color = match p {
                ColorPalette::XTerm256 => ColorMode::XTerm256,
                ColorPalette::Grayscale => ColorMode::Grayscale,
            };
            ws.focused_window_mut().color_mode = color;
            ws.focused_window_mut().dirty = true;
            set_message(
                app,
                format!(
                    "colors {}",
                    match p {
                        ColorPalette::XTerm256 => "xterm256",
                        ColorPalette::Grayscale => "gray",
                    }
                ),
            );
        }
        other => {
            ws.apply_command(other)?;
        }
    }
    app.full_repaint = true;
    Ok(())
}

fn set_message(app: &mut AppState, msg: String) {
    app.message = Some(msg);
    app.message_expires = Some(Instant::now() + Duration::from_secs(2));
    app.chrome_dirty = true;
}

/// Move the palette's selection highlight forward (or backward, if
/// `reverse`). The input buffer itself is left alone — a subsequent
/// Enter commits the selection into `input` without executing.
///
/// Shared by `Tab`/`Shift-Tab`, `Down`/`Up`, and `Ctrl-n`/`Ctrl-p`.
fn cycle_completion(
    input: &str,
    completion_idx: &mut Option<usize>,
    registry: &CommandRegistry,
    reverse: bool,
) {
    let completions = compute_completions(input, registry);
    if completions.is_empty() {
        *completion_idx = None;
        return;
    }
    let idx = match *completion_idx {
        None => {
            if reverse {
                completions.len() - 1
            } else {
                0
            }
        }
        Some(i) => {
            if reverse {
                if i == 0 {
                    completions.len() - 1
                } else {
                    i - 1
                }
            } else {
                (i + 1) % completions.len()
            }
        }
    };
    *completion_idx = Some(idx);
}

/// One row in the command palette's completion list.
#[derive(Debug, Clone)]
struct PaletteCompletion {
    /// What the user sees (e.g. `":split  — horizontal split …"` or
    /// `"foo.pdf"`).
    display: String,
    /// What to paste into `input` when this entry is selected.
    insert: String,
    /// Byte index in `input` where `insert` replaces from.
    replace_from: usize,
}

/// Build completions for the palette's current input. Switches between
/// command-name completion and filesystem-path completion when the
/// user has typed one of the path-taking commands followed by a
/// space.
fn compute_completions(input: &str, registry: &CommandRegistry) -> Vec<PaletteCompletion> {
    // If there's a whitespace in the input, we might be typing an
    // argument to a command.
    let ws_pos = input.find(char::is_whitespace);
    if let Some(pos) = ws_pos {
        let name = &input[..pos];
        if is_path_command(name) {
            let arg_start = pos + 1;
            let arg = &input[arg_start..];
            return path_completions(arg, arg_start);
        }
    }
    // Otherwise, command-name completion. Replace from position 0.
    registry
        .complete(input)
        .into_iter()
        .map(|c| {
            let mut insert = c.name.to_string();
            // For commands that take args, leave a trailing space so
            // the user can immediately type the argument.
            if !matches!(c.arg, svreader_core::CommandArg::None) {
                insert.push(' ');
            }
            PaletteCompletion {
                display: format!(":{}  — {}", c.name, c.description),
                insert,
                replace_from: 0,
            }
        })
        .collect()
}

fn is_path_command(name: &str) -> bool {
    matches!(
        name,
        "edit"
            | "e"
            | "open"
            | "o"
            | "split"
            | "sp"
            | "vsplit"
            | "vsp"
            | "tabnew"
            | "tabe"
    )
}

/// Directory-listing completion for a partial path. `replace_from`
/// is the byte position in the palette input where replacements
/// should start.
fn path_completions(arg: &str, replace_from: usize) -> Vec<PaletteCompletion> {
    let (dir_part, prefix) = split_path_prefix(arg);
    let dir = expand_home_path(&dir_part);
    let dir_path: &std::path::Path = std::path::Path::new(&dir);
    let read = match std::fs::read_dir(dir_path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut entries: Vec<(String, bool)> = Vec::new();
    for e in read.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        let is_dir = e
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false);
        // Only show directories (so the user can drill into them) and
        // files svreader can actually open. Other files would parse-
        // fail on `:open`, so hiding them keeps the palette useful.
        if !is_dir && !is_supported_document(&name) {
            continue;
        }
        // Hide koreader-style `<file>.sdr` sidecar directories — they
        // only hold `metadata.pdf.lua` and are never something the
        // user wants to open.
        if is_dir && name.ends_with(".sdr") {
            continue;
        }
        entries.push((name, is_dir));
    }
    // Directories first, then files; alphabetical within each group.
    entries.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });
    entries
        .into_iter()
        .map(|(name, is_dir)| {
            let mut insert = dir_part.clone();
            insert.push_str(&name);
            let mut display = name.clone();
            if is_dir {
                insert.push('/');
                display.push('/');
            }
            PaletteCompletion {
                display,
                insert,
                replace_from,
            }
        })
        .collect()
}

/// Extensions svreader can open. Extend this list as the Document
/// backends grow (M4: EPUB, DjVu, CBZ).
const SUPPORTED_EXTS: &[&str] = &["pdf"];

fn is_supported_document(name: &str) -> bool {
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        return false;
    }
    let lower = ext.to_ascii_lowercase();
    SUPPORTED_EXTS.iter().any(|e| *e == lower)
}

/// Split `"foo/bar/ba"` into `("foo/bar/", "ba")`. For inputs without
/// any `/`, dir is `""` (caller resolves to CWD).
fn split_path_prefix(arg: &str) -> (String, String) {
    if let Some(pos) = arg.rfind('/') {
        (arg[..pos + 1].to_string(), arg[pos + 1..].to_string())
    } else {
        (String::new(), arg.to_string())
    }
}

/// Expand a leading `~/` or `~` to `$HOME`. If the input is empty
/// returns `./`.
fn expand_home_path(dir: &str) -> String {
    if dir.is_empty() {
        return "./".to_string();
    }
    if dir == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return home.to_string_lossy().into_owned();
        }
    }
    if let Some(rest) = dir.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = std::path::PathBuf::from(home);
            p.push(rest);
            return p.to_string_lossy().into_owned();
        }
    }
    dir.to_string()
}

fn crossterm_to_key(k: KeyEvent) -> Option<Key> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);

    // Arrows with modifiers map to focus / resize.
    let arrow = match k.code {
        KeyCode::Left => Some(ArrowDir::Left),
        KeyCode::Right => Some(ArrowDir::Right),
        KeyCode::Up => Some(ArrowDir::Up),
        KeyCode::Down => Some(ArrowDir::Down),
        _ => None,
    };
    if let Some(d) = arrow {
        if shift && alt {
            return Some(Key::ShiftAltArrow(d));
        }
        if alt {
            return Some(Key::AltArrow(d));
        }
        return Some(match d {
            ArrowDir::Left => Key::Left,
            ArrowDir::Right => Key::Right,
            ArrowDir::Up => Key::Up,
            ArrowDir::Down => Key::Down,
        });
    }

    // Ctrl-PageUp / Ctrl-PageDown → switch tabs.
    // Ctrl-Shift-PageUp / Ctrl-Shift-PageDown → reorder tabs.
    if let KeyCode::PageUp | KeyCode::PageDown = k.code {
        let d = if matches!(k.code, KeyCode::PageUp) {
            PageDir::Up
        } else {
            PageDir::Down
        };
        if ctrl && shift {
            return Some(Key::CtrlShiftPage(d));
        }
        if ctrl {
            return Some(Key::CtrlPage(d));
        }
    }

    let key = match k.code {
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Char(c) if ctrl => Key::Ctrl(c),
        KeyCode::Char(c) => Key::Char(c),
        _ => return None,
    };
    Some(key)
}

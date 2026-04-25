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
use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use svreader_core::cache::CacheKey;
use svreader_core::keys::{ArrowDir, Key, KeyOutcome, KeyParser, KeyParserState, PageDir};
use svreader_core::{
    Action, Buffer, ColorPalette, CommandRegistry, Document, ExplorerBuffer, ExplorerKind,
    Highlights, Outline, PageMetrics, ParsedCommand, PrefetchRequest, RenderCache, Renderer,
    Rotation, Viewport, ZoomMode,
};

use crate::capabilities::{probe_sixel, SIXEL_TERMINALS};
use crate::ecache_fill::{EncCacheFiller, RefillRequest};
use crate::encoded_cache::{ComposedEncodedCache, EncodedFrame, EncodedKey};
use crate::sixel_write::{blank_rect, emit_dcs, encode_rgba_to_dcs, ColorMode};
use crate::terminal::{self, TermGeom};
use crate::timings::{FrameTiming, TimingsLog};
use crate::window::{CellRect, WindowId};
use crate::workspace::Workspace;
use crate::RunOptions;

const STATUS_ROWS: u16 = 1;
const PALETTE_MAX_ROWS: u16 = 6;
const HELP_ROWS: u16 = 26;

#[derive(Debug, Clone)]
enum Mode {
    Normal,
    Command {
        input: String,
        completion_idx: Option<usize>,
    },
    /// Live search prompt (`/` forward, `?` backward). Pressing Enter
    /// runs the search across the focused PDF; Esc bails without
    /// touching existing highlights.
    Search {
        input: String,
        forward: bool,
    },
    Help,
    /// Outline / table-of-contents picker over the focused PDF.
    Toc {
        entries: Vec<TocEntry>,
        selected: usize,
        scroll: usize,
        pending: KeyParserState,
    },
    /// Bookmark list (`:marks` / `:bookmarks`).
    Marks {
        entries: Vec<MarkEntry>,
        selected: usize,
        scroll: usize,
        pending: KeyParserState,
    },
}

#[derive(Debug, Clone)]
struct TocEntry {
    depth: usize,
    title: String,
    page: usize,
}

#[derive(Debug, Clone, Copy)]
struct MarkEntry {
    mark: char,
    page: usize,
    x_off: i32,
    y_off: i32,
}

pub fn run(opts: RunOptions) -> Result<()> {
    let pdf_path = opts.pdf.clone();

    enable_raw_mode().context("enable_raw_mode failed")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide, EnableFocusChange)
        .context("alt-screen enter failed")?;

    let res = run_inner(opts, pdf_path, &mut stdout);

    let _ = execute!(stdout, DisableFocusChange);
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
    res
}

fn startup_message(pdf_path: Option<&PathBuf>) -> String {
    match pdf_path {
        Some(p) if p.is_dir() => format!("svreader — opening explorer at {}...\r\n", p.display()),
        Some(p) => format!("svreader — loading {}...\r\n", p.display()),
        None => "svreader — opening explorer...\r\n".into(),
    }
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
    /// Whether mouse capture is currently on. Mirrors what the
    /// terminal has been told via Enable/DisableMouseCapture.
    mouse_enabled: bool,
}

fn run_inner(
    opts: RunOptions,
    pdf_path: Option<PathBuf>,
    stdout: &mut io::Stdout,
) -> Result<()> {
    // Loading banner before anything slow.
    write!(stdout, "\x1b[2J\x1b[H")?;
    write!(stdout, "{}", startup_message(pdf_path.as_ref()))?;
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

    let cache = Arc::new(RenderCache::new(5));
    // ECache lives above the RenderCache. Default 10 — small enough
    // that we don't hold too many encoded strings in memory, big
    // enough to cover ±4 pages plus the current one without
    // evicting the current entry while the filler populates the
    // neighbourhood.
    let ecache = Arc::new(ComposedEncodedCache::new(10));
    let ecache_filler = Arc::new(EncCacheFiller::spawn(cache.clone(), ecache.clone())?);
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
    let mut ws = match pdf_path.as_ref() {
        // A directory → open explorer rooted there.
        Some(p) if p.is_dir() => Workspace::with_explorer(
            cache.clone(),
            ecache.clone(),
            ecache_filler.clone(),
            p,
            initial_viewport,
        )?,
        Some(p) => Workspace::with_pdf(
            cache.clone(),
            ecache.clone(),
            ecache_filler.clone(),
            p,
            initial_viewport,
        )?,
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            Workspace::with_explorer(
                cache.clone(),
                ecache.clone(),
                ecache_filler.clone(),
                &cwd,
                initial_viewport,
            )?
        }
    };

    // Apply the start-page override (after DocState load). Only
    // meaningful when opened directly on a PDF; when booted into the
    // explorer, the focused buffer is an ExplorerBuffer and this is a
    // no-op.
    if let Some(page) = opts.start_page {
        let idx = page.saturating_sub(1);
        let buf_id = ws.focused_window().buffer;
        if let Some(buf) = ws.buffer_pdf_mut(buf_id) {
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

    let initial_mouse = {
        let buf_id = ws.focused_window().buffer;
        ws.buffer_pdf(buf_id)
            .and_then(|b| b.state.mouse_enabled)
            .unwrap_or(true)
    };
    let mut app = AppState {
        key_state: KeyParserState::default(),
        mode: Mode::Normal,
        pending_hint: String::new(),
        message: None,
        message_expires: None,
        chrome_dirty: true,
        full_repaint: true,
        mouse_enabled: initial_mouse,
    };
    if app.mouse_enabled {
        let _ = execute!(stdout, EnableMouseCapture);
    }

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

        // Tell the RCache what page the user is on so eviction keeps
        // the active neighbourhood. Without this, prefetching ±2
        // past the edges of the cache could evict the page we're
        // looking at.
        {
            let focused = ws.focused_window();
            ws.cache
                .set_focus(focused.buffer, focused.viewport.page_idx);
        }

        // Paint windows.
        paint_windows(stdout, &mut ws, &cache, &ecache, &timings_log, &layout, geom)?;

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
            Mode::Search { input, forward } => {
                let bottom = geom.rows.saturating_sub(STATUS_ROWS);
                let input_row = bottom.saturating_sub(1);
                draw_search_prompt(stdout, input_row, geom.cols, input, *forward)?;
                let col = (input.chars().count() as u16).saturating_add(2);
                write!(stdout, "\x1b[{};{}H", input_row + 1, col)?;
                execute!(stdout, cursor::Show)?;
            }
            Mode::Help => {
                let bottom = geom.rows.saturating_sub(STATUS_ROWS);
                let top = bottom.saturating_sub(HELP_ROWS);
                draw_help(stdout, top, bottom, geom.cols)?;
            }
            Mode::Toc {
                entries,
                selected,
                scroll,
                ..
            } => {
                let body = body_rect(geom, if ws.tab_count() > 1 { 1 } else { 0 });
                draw_toc_overlay(stdout, body, entries, *selected, *scroll)?;
            }
            Mode::Marks {
                entries,
                selected,
                scroll,
                ..
            } => {
                let body = body_rect(geom, if ws.tab_count() > 1 { 1 } else { 0 });
                draw_marks_overlay(stdout, body, entries, *selected, *scroll)?;
            }
        }
        stdout.flush()?;

        // Fire prefetches around the focused window's page.
        fire_prefetches(&mut ws);
        // Kick the ECache filler around the focused frame. It'll
        // only encode pages that are already in the RCache — it
        // never renders — and stops early if a newer request
        // arrives (i.e. the user moved again before it finished).
        fire_ecache_refill(&ws);

        // Drain anything that arrived during paint before we block
        // again. If the user hammered j five times while we were
        // painting, we want to process all five keys first and only
        // paint once, showing the final state — not render five
        // intermediate frames in sequence.
        drain_pending_events(
            stdout,
            &mut ws,
            &cache,
            &ecache,
            &registry,
            &mut app,
            &mut geom,
            &opts,
        )?;

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
                // Encoded frames depend on screen dims, so a resize
                // makes every entry in the ECache stale.
                ecache.clear();
                app.full_repaint = true;
            }
            Event::Key(k) => {
                if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                execute!(stdout, cursor::Hide)?;
                match app.mode.clone() {
                    Mode::Normal => handle_normal_key(&mut ws, &mut app, k, &ecache)?,
                    Mode::Command { .. } => {
                        handle_command_key(&mut ws, &cache, &ecache, &registry, &mut app, k, stdout)?;
                    }
                    Mode::Search { .. } => {
                        handle_search_key(&mut ws, &ecache, &mut app, k)?;
                    }
                    Mode::Help => {
                        if matches!(k.code, KeyCode::Esc) || k.code == KeyCode::Char('q') {
                            app.mode = Mode::Normal;
                            app.full_repaint = true;
                        }
                    }
                    Mode::Toc { .. } => handle_toc_key(&mut ws, &mut app, k)?,
                    Mode::Marks { .. } => handle_marks_key(&mut ws, &mut app, k)?,
                }
            }
            Event::Mouse(m) => {
                if app.mouse_enabled && matches!(app.mode, Mode::Normal) {
                    handle_mouse(&mut ws, &mut app, m, geom)?;
                }
            }
            // tmux drops sixel images from inactive panes, so when the
            // pane regains focus we force a repaint to re-emit them.
            Event::FocusGained => {
                app.full_repaint = true;
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
    cache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
    timings_log: &TimingsLog,
    layout: &[(WindowId, CellRect)],
    geom: TermGeom,
) -> Result<()> {
    // Flatten layout so we can mutate windows without fighting the
    // tree borrow.
    let layout_map: Vec<(WindowId, CellRect)> = layout.iter().copied().collect();
    for (id, rect) in layout_map {
        let (dirty, is_explorer) = {
            let Some(w) = ws.current_tab().tree.find(id) else {
                continue;
            };
            let is_explorer = ws
                .buffer(w.buffer)
                .map(Buffer::is_explorer)
                .unwrap_or(false);
            (w.dirty || w.last_rect != Some(rect), is_explorer)
        };
        if !dirty {
            continue;
        }
        if is_explorer {
            paint_explorer_window(stdout, ws, id, rect, geom)?;
        } else {
            paint_window(stdout, ws, cache, ecache, timings_log, id, rect, geom)?;
        }
    }
    Ok(())
}

fn paint_window(
    stdout: &mut impl Write,
    ws: &mut Workspace,
    cache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
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

    let Some(buf) = ws.buffer_pdf(buffer_id) else {
        return Ok(());
    };

    // Compose viewport snapshot. Borrowed from the tree.
    let (display_scale, raster_scale, viewport, rotation, page_idx, color_mode) = {
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
            w.color_mode,
        )
    };

    // Build highlights for this page from the buffer's active search.
    // Stays empty when no search is running — `compose_with_highlights`
    // does nothing in that case.
    let page_size_for_hl = buf.pdf.page_size(page_idx)?;
    let highlights = if buf.search.is_active() {
        Some(Highlights::from_matches(
            &buf.search.matches,
            page_idx,
            page_size_for_hl,
            buf.search.current,
        ))
    } else {
        None
    };
    let has_highlights = highlights.as_ref().map(|h| !h.is_empty()).unwrap_or(false);

    let rkey = CacheKey::new(buffer_id, page_idx, display_scale, raster_scale, rotation);
    let ekey =
        EncodedKey::from_viewport(buffer_id, &viewport, display_scale, raster_scale, color_mode);

    // Cancel checkpoint: if the ECache already has this exact view
    // we always finish (the fast path is microseconds). But for the
    // slow path, we first check if the user already pressed another
    // key — if so, bail out with dirty=true so the outer loop can
    // process the input and repaint the new target directly. Keeps
    // us from spending 100+ ms encoding a frame the user no longer
    // cares about.
    let ecache_hit_available = !has_highlights && ecache.get(&ekey).is_some();
    if !ecache_hit_available && has_pending_event() {
        // Leave window state untouched (keeps dirty=true). The outer
        // loop will process the pending keys and re-enter paint.
        return Ok(());
    }

    let t_overall = Instant::now();

    // Fast path: ECache hit → emit the pre-encoded DCS directly.
    // Slow path: produce the encoded frame by routing through the
    // RenderCache (single-flight'd → no duplicate mupdf renders),
    // then compose + encode. ECache has its own single-flight so
    // two paint calls for the identical viewport don't re-encode.
    //
    // When this page has search highlights, the ECache key doesn't
    // capture the highlight state — bypass the ECache entirely so a
    // new compose+encode runs each time. (ECache is cleared on every
    // search-state change, so stale entries can't leak in either.)
    let (frame, render_dur, compose_dur, encode_dur) = if has_highlights {
        let render_was_hot = cache.contains(&rkey);
        let t_path = Instant::now();
        let pdf = &buf.pdf;
        let (page, _page_render_dur) = cache.get_or_render(rkey, || {
            let (page, rt) = Renderer::render_page(pdf, &viewport)?;
            Ok((page, rt.render))
        })?;
        let (composed, ct) = Renderer::compose_with_highlights(&page, &viewport, highlights.as_ref());
        let (dcs, encode_dur) = encode_rgba_to_dcs(&composed, color_mode)?;
        let frame = Arc::new(EncodedFrame {
            dcs,
            pixel_height: composed.height(),
        });
        let total = t_path.elapsed();
        let render_d = if render_was_hot {
            Duration::ZERO
        } else {
            total.saturating_sub(ct.compose + encode_dur)
        };
        (frame, render_d, ct.compose, encode_dur)
    } else {
        // Pre-check: is the raster already cached? Used only to
        // attribute timing ("did this call cost a render?"); the
        // real guard against duplicate work is get_or_render.
        let render_was_hot = cache.contains(&rkey);
        let t_cache_path = Instant::now();
        let (frame, compose_d, encode_d) = ecache.get_or_encode(ekey, || {
            let pdf = &buf.pdf;
            let (page, _page_render_dur) = cache.get_or_render(rkey, || {
                let (page, rt) = Renderer::render_page(pdf, &viewport)?;
                Ok((page, rt.render))
            })?;
            let (composed, ct) = Renderer::compose(&page, &viewport);
            let (dcs, encode_dur) = encode_rgba_to_dcs(&composed, color_mode)?;
            let frame = EncodedFrame {
                dcs,
                pixel_height: composed.height(),
            };
            Ok((frame, ct.compose, encode_dur))
        })?;
        // On an ECache hit the render/compose/encode are all zero.
        // On an ECache miss where the raster was already in RCache,
        // bill the time to "compose+encode" not to "render".
        let total_for_cache_path = t_cache_path.elapsed();
        let render_dur = if render_was_hot || compose_d == Duration::ZERO {
            Duration::ZERO
        } else {
            // Time spent inside get_or_render that wasn't compose+encode
            // is the render portion.
            total_for_cache_path.saturating_sub(compose_d + encode_d)
        };
        (frame, render_dur, compose_d, encode_d)
    };

    let (write_dur, _bytes) = emit_dcs(&frame.dcs, image_rect.col, image_rect.row, stdout)?;

    let total = t_overall.elapsed();
    let attributed = render_dur + compose_dur + encode_dur + write_dur;
    let other = total.saturating_sub(attributed);
    let timing = FrameTiming {
        render: render_dur,
        compose: compose_dur,
        encode: encode_dur,
        write: write_dur,
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
    w.last_sixel_rows = frame.pixel_height.div_ceil(geom.cell_px_h as u32) as u16;
    w.last_rect = Some(rect);
    w.dirty = false;
    Ok(())
}

fn paint_explorer_window(
    stdout: &mut impl Write,
    ws: &mut Workspace,
    id: WindowId,
    rect: CellRect,
    geom: TermGeom,
) -> Result<()> {
    // Blank the previous rect if it moved so stale text doesn't hang
    // off the new window edge.
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

    // Update screen dims so a later swap to a PDF has correct size.
    let (img_w, img_h) = pixel_size(rect, geom);
    let _ = ws.resync_window_viewport(id, img_w, img_h)?;

    // Always clear the window's rect first so shrinking entry lists
    // don't leave orphan rows behind.
    blank_rect(rect.col, rect.row, rect.cols, rect.rows, stdout).ok();

    let Some(Buffer::Explorer(ex)) = ws.buffer(buffer_id) else {
        return Ok(());
    };
    draw_explorer(stdout, ex, rect)?;

    let w = ws.current_tab_mut().tree.find_mut(id).unwrap();
    w.last_rect = Some(rect);
    w.dirty = false;
    Ok(())
}

/// Draw an explorer buffer into `rect`. Header on the first row,
/// entries below with the selected entry highlighted.
fn draw_explorer(
    stdout: &mut impl Write,
    ex: &ExplorerBuffer,
    rect: CellRect,
) -> Result<()> {
    let cols = rect.cols as usize;
    if cols == 0 || rect.rows == 0 {
        return Ok(());
    }

    // Header row: current working directory, dim grey.
    let header_raw = ex.cwd.to_string_lossy().into_owned();
    let header: String = truncate_cols(&header_raw, cols);
    write!(
        stdout,
        "\x1b[{};{}H\x1b[38;5;244m{}\x1b[0m",
        rect.row + 1,
        rect.col + 1,
        header
    )?;

    // Entry rows.
    let list_rows = (rect.rows as usize).saturating_sub(1);
    if list_rows == 0 {
        return Ok(());
    }
    let total = ex.entries.len();
    // Scroll so the selected entry stays visible.
    let scroll = if ex.selected >= list_rows {
        ex.selected + 1 - list_rows
    } else {
        0
    };
    let visible = list_rows.min(total.saturating_sub(scroll));
    for i in 0..visible {
        let abs = scroll + i;
        let Some(entry) = ex.entries.get(abs) else {
            break;
        };
        let row = rect.row + 1 + i as u16;
        let selected = abs == ex.selected;
        let (style, label) = match entry.kind {
            ExplorerKind::ParentDir => ("\x1b[1;38;5;110m".to_string(), format!("{}/", entry.name)),
            ExplorerKind::Dir => ("\x1b[1;38;5;110m".to_string(), format!("{}/", entry.name)),
            ExplorerKind::Pdf => ("\x1b[38;5;252m".to_string(), entry.name.clone()),
        };
        let truncated = truncate_cols(&label, cols);
        write!(stdout, "\x1b[{};{}H", row + 1, rect.col + 1)?;
        if selected {
            write!(stdout, "\x1b[7m")?;
        }
        write!(stdout, "{}{}\x1b[0m", style, truncated)?;
        if selected {
            write!(stdout, "\x1b[0m")?;
        }
    }

    Ok(())
}

/// Truncate a string to at most `cols` display columns. We use the
/// char count as a cheap approximation; the explorer doesn't render
/// CJK or emoji filenames any better than that.
fn truncate_cols(s: &str, cols: usize) -> String {
    s.chars().take(cols).collect()
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
    let rcache_stats = ws.cache.stats();
    let ecache_stats = ws.ecache.stats();

    let mut s = String::new();
    match buf {
        Some(Buffer::Explorer(ex)) => {
            let count = ex.entries.len();
            s.push_str(&format!(
                " {} | {}/{} ",
                ex.display_name(),
                if count == 0 { 0 } else { ex.selected + 1 },
                count
            ));
            if let Some(e) = ex.selected_entry() {
                s.push_str(&format!("| {}", e.name));
            }
        }
        _ => {
            let (file_name, page_count) = match buf {
                Some(Buffer::Pdf(p)) => (p.display_name(), p.pdf.page_count()),
                _ => ("document".to_string(), 1),
            };
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
            s.push_str(&format!(
                " RCache:{}/{} ECache:{}/{}",
                rcache_stats.0, rcache_stats.1, ecache_stats.0, ecache_stats.1
            ));
            if let Some(Buffer::Pdf(pdf)) = buf {
                if pdf.search.is_active() {
                    let cur = pdf.search.current.map(|i| i + 1).unwrap_or(0);
                    s.push_str(&format!(
                        " /{} [{}/{}]",
                        pdf.search.query,
                        cur,
                        pdf.search.matches.len()
                    ));
                }
            }
            if let Some(t) = &focused.last_timing {
                s.push(' ');
                s.push_str(&t.short_label());
            }
        }
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
        "   ↓ / ↑         fine scroll down/up (10% of viewport)",
        "   Ctrl-d/u      half-screen down/up",
        "   Ctrl-f/b      next/prev page (no overlap)",
        "   gg / G        first / last page",
        "   H M L         page top / middle / bottom",
        "   h / l         scroll left / right",
        "   w / e / f     fit width / height / page",
        "   + / -         zoom in / out",
        "   r / R         rotate CW / CCW",
        "   n             next match (or toggle night when no search)",
        "   /  /  ?       search forward / backward",
        "   N             previous match",
        "   Esc           clear search highlights",
        "   t             open table of contents",
        "   m{a-z}        set bookmark   '{a-z}   jump to bookmark",
        "   Ctrl-o        jump back (jump list)",
        "   click link    follow internal PDF link (mouse)",
        "   Ctrl-w h/j/k/l   move focus",
        "   Ctrl-w s / v     split horizontal / vertical",
        "   Ctrl-w c / o     close / only",
        "   gt / gT       next / previous tab",
        "   Ctrl-^        alternate buffer",
        "   (in :Ex)      j/k select, Enter/l open, -/u/h/Backspace parent",
        "   :             command palette  (:toc, :text, :marks, :mouse, ...)",
        "   :help         show this help   q   quit",
        "",
        " Press q or Esc to close.",
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

fn fire_ecache_refill(ws: &Workspace) {
    if !ws.ecache.enabled() {
        return;
    }
    let focused = ws.focused_window();
    let Some(buf) = ws.buffer_pdf(focused.buffer) else {
        return;
    };
    if buf.page_info.page_count() == 0 {
        return;
    }
    // Radius = how many Navigator steps to walk in each direction.
    // Capping at (capacity-1)/2 keeps the filler from evicting the
    // current page while populating neighbours.
    let (_, cap) = ws.ecache.stats();
    let radius = cap.saturating_sub(1) / 2;
    if radius == 0 {
        return;
    }
    let req = RefillRequest::new(
        focused.buffer,
        focused.viewport.clone(),
        focused.color_mode,
        buf.page_info.clone(),
        radius,
    );
    ws.ecache_filler.request(req);
}

/// Peek crossterm's event queue without blocking. True means the
/// main thread has unread input to process. Used inside
/// `paint_window` as a cancel checkpoint — if a key is already
/// waiting, we skip the expensive compose+encode so the user's
/// keypress gets applied sooner.
fn has_pending_event() -> bool {
    event::poll(Duration::ZERO).unwrap_or(false)
}

/// Drain every pending event off the crossterm queue and apply each
/// one. No painting in between — the outer loop will paint once
/// after this returns, showing the final state.
#[allow(clippy::too_many_arguments)]
fn drain_pending_events(
    stdout: &mut io::Stdout,
    ws: &mut Workspace,
    cache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
    registry: &CommandRegistry,
    app: &mut AppState,
    geom: &mut TermGeom,
    opts: &RunOptions,
) -> Result<()> {
    while event::poll(Duration::ZERO)? {
        let ev = event::read()?;
        match ev {
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
                *geom = new_geom;
                let tab_bar_rows: u16 = if ws.tab_count() > 1 { 1 } else { 0 };
                let body = body_rect(*geom, tab_bar_rows);
                ws.propagate_geometry(geom.cell_px_w, geom.cell_px_h, body);
                cache.clear();
                ecache.clear();
                app.full_repaint = true;
            }
            Event::Key(k) => {
                if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                match app.mode.clone() {
                    Mode::Normal => handle_normal_key(ws, app, k, ecache)?,
                    Mode::Command { .. } => {
                        handle_command_key(ws, cache, ecache, registry, app, k, stdout)?;
                    }
                    Mode::Search { .. } => {
                        handle_search_key(ws, ecache, app, k)?;
                    }
                    Mode::Help => {
                        if matches!(k.code, KeyCode::Esc) || k.code == KeyCode::Char('q') {
                            app.mode = Mode::Normal;
                            app.full_repaint = true;
                        }
                    }
                    Mode::Toc { .. } => handle_toc_key(ws, app, k)?,
                    Mode::Marks { .. } => handle_marks_key(ws, app, k)?,
                }
            }
            Event::Mouse(m) => {
                if app.mouse_enabled && matches!(app.mode, Mode::Normal) {
                    handle_mouse(ws, app, m, *geom)?;
                }
            }
            Event::FocusGained => {
                app.full_repaint = true;
            }
            _ => {}
        }
    }
    Ok(())
}

fn fire_prefetches(ws: &mut Workspace) {
    if !ws.cache.enabled() {
        return;
    }
    let focused_buffer = ws.focused_window().buffer;
    let Some(buf) = ws.buffer_pdf(focused_buffer) else {
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

fn handle_normal_key(
    ws: &mut Workspace,
    app: &mut AppState,
    k: KeyEvent,
    ecache: &Arc<ComposedEncodedCache>,
) -> Result<()> {
    if matches!(k.code, KeyCode::Char('c')) && k.modifiers.contains(KeyModifiers::CONTROL) {
        ws.quit_requested = true;
        return Ok(());
    }

    // Explorer windows get first dibs on plain keys (no modifiers, or
    // just Shift) for list navigation. Anything with Ctrl/Alt, plus
    // `:`, `?`, `q`, and the `<C-w>` chord still go through the
    // regular KeyParser below so window operations keep working from
    // inside an explorer.
    if ws.focused_is_explorer() {
        let mods = k.modifiers;
        let plain = mods.is_empty() || mods == KeyModifiers::SHIFT;
        if plain {
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    ws.explorer_move(1);
                    return Ok(());
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    ws.explorer_move(-1);
                    return Ok(());
                }
                KeyCode::Char('g') => {
                    // `gg` inside the explorer jumps to first. Use
                    // the existing `g` leader so a single `g` still
                    // pends.
                    if app.key_state.leader == svreader_core::keys::Leader::G {
                        app.key_state.clear();
                        ws.explorer_first();
                    } else {
                        app.key_state.leader = svreader_core::keys::Leader::G;
                    }
                    app.pending_hint = app.key_state.hint();
                    return Ok(());
                }
                KeyCode::Char('G') => {
                    app.key_state.clear();
                    ws.explorer_last();
                    return Ok(());
                }
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                    if let Err(e) = ws.explorer_activate() {
                        set_message(app, format!("{e}"));
                    }
                    return Ok(());
                }
                KeyCode::Backspace
                | KeyCode::Left
                | KeyCode::Char('h')
                | KeyCode::Char('-')
                | KeyCode::Char('u') => {
                    if let Err(e) = ws.explorer_parent() {
                        set_message(app, format!("{e}"));
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
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
        KeyOutcome::OpenSearch { forward } => {
            app.mode = Mode::Search {
                input: String::new(),
                forward,
            };
        }
        KeyOutcome::SearchStep { forward } => {
            // If a search is active, n/N step through hits; otherwise
            // fall back to the legacy meaning (n = night toggle, N = no-op).
            let buf_id = ws.focused_window().buffer;
            let has_active_search = ws
                .buffer_pdf(buf_id)
                .map(|b| b.search.is_active())
                .unwrap_or(false);
            if has_active_search {
                step_search(ws, app, ecache, forward)?;
            } else if forward {
                ws.apply_nav(Action::ToggleNight, 1)?;
            }
        }
        KeyOutcome::Cancel => {
            // Esc with no pending state: drop active search highlights.
            let buf_id = ws.focused_window().buffer;
            let cleared = ws
                .buffer_pdf_mut(buf_id)
                .map(|b| b.clear_search())
                .unwrap_or(false);
            if cleared {
                ecache.clear();
                ws.focused_window_mut().dirty = true;
                set_message(app, "search cleared".into());
            }
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
        KeyOutcome::ToggleToc => {
            if let Err(e) = enter_toc_mode(ws, app) {
                set_message(app, format!("{e}"));
            }
        }
        KeyOutcome::SetMark(c) => {
            if let Err(e) = ws.set_bookmark(c) {
                set_message(app, format!("mark: {e}"));
            } else {
                set_message(app, format!("mark '{c}' set"));
            }
        }
        KeyOutcome::JumpMark(c) => match ws.jump_bookmark(c) {
            Ok(true) => app.full_repaint = true,
            Ok(false) => set_message(app, format!("mark '{c}' not set")),
            Err(e) => set_message(app, format!("jump-mark: {e}")),
        },
        KeyOutcome::JumpBack => match ws.jump_back() {
            Ok(true) => app.full_repaint = true,
            Ok(false) => set_message(app, "jump list empty".into()),
            Err(e) => set_message(app, format!("back: {e}")),
        },
        KeyOutcome::JumpForward => match ws.jump_forward() {
            Ok(true) => app.full_repaint = true,
            Ok(false) => set_message(app, "no forward jump".into()),
            Err(e) => set_message(app, format!("forward: {e}")),
        },
    }
    Ok(())
}

fn handle_command_key(
    ws: &mut Workspace,
    cache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
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
                if let Err(e) = execute_command(ws, cache, ecache, app, registry, &line) {
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
    cache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
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
            set_message(app, format!("RCache {}", if b { "on" } else { "off" }));
        }
        ParsedCommand::CacheToggle => {
            cache.set_enabled(!cache.enabled());
            set_message(
                app,
                format!("RCache {}", if cache.enabled() { "on" } else { "off" }),
            );
        }
        ParsedCommand::CacheSize(n) => {
            cache.resize(n);
            set_message(app, format!("RCache size {}", n));
        }
        ParsedCommand::ECacheSet(b) => {
            ecache.set_enabled(b);
            set_message(app, format!("ECache {}", if b { "on" } else { "off" }));
        }
        ParsedCommand::ECacheToggle => {
            ecache.set_enabled(!ecache.enabled());
            set_message(
                app,
                format!("ECache {}", if ecache.enabled() { "on" } else { "off" }),
            );
        }
        ParsedCommand::ECacheSize(n) => {
            ecache.resize(n);
            set_message(app, format!("ECache size {}", n));
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
            ecache.clear();
        }
        ParsedCommand::CopyPage => {
            match copy_current_page_to_clipboard(ws) {
                Ok(tool) => set_message(app, format!("copied page to clipboard ({tool})")),
                Err(e) => set_message(app, format!("copy failed: {e}")),
            }
        }
        ParsedCommand::OpenTextEditor => {
            match open_document_text_in_editor(ws) {
                Ok(()) => {
                    // Editor took over the terminal; everything's
                    // potentially trashed — request a full repaint.
                    app.full_repaint = true;
                }
                Err(e) => set_message(app, format!(":text: {e}")),
            }
        }
        ParsedCommand::ToggleToc => {
            enter_toc_mode(ws, app)?;
        }
        ParsedCommand::ToggleMarks => {
            enter_marks_mode(ws, app)?;
        }
        ParsedCommand::DeleteMark(c) => {
            if ws.delete_bookmark(c) {
                set_message(app, format!("mark '{c}' deleted"));
            } else {
                set_message(app, format!("no mark '{c}'"));
            }
        }
        ParsedCommand::JumpBack => match ws.jump_back() {
            Ok(true) => {}
            Ok(false) => set_message(app, "jump list empty".into()),
            Err(e) => set_message(app, format!("back: {e}")),
        },
        ParsedCommand::JumpForward => match ws.jump_forward() {
            Ok(true) => {}
            Ok(false) => set_message(app, "no forward jump".into()),
            Err(e) => set_message(app, format!("forward: {e}")),
        },
        ParsedCommand::MouseSet(b) => {
            set_mouse_capture(app, b);
            ws.set_mouse_pref(Some(b));
            set_message(app, format!("mouse {}", if b { "on" } else { "off" }));
        }
        ParsedCommand::MouseToggle => {
            let new_val = !app.mouse_enabled;
            set_mouse_capture(app, new_val);
            ws.set_mouse_pref(Some(new_val));
            set_message(app, format!("mouse {}", if new_val { "on" } else { "off" }));
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
    // If the command put us into a fullscreen-ish overlay (TOC /
    // marks), skip the full repaint — the overlay clears its own
    // rect and a forced sixel re-emit underneath it just flashes.
    // Help also overlays but only over the bottom strip, so a
    // repaint there is harmless.
    if !matches!(app.mode, Mode::Toc { .. } | Mode::Marks { .. }) {
        app.full_repaint = true;
    }
    Ok(())
}

fn set_message(app: &mut AppState, msg: String) {
    app.message = Some(msg);
    app.message_expires = Some(Instant::now() + Duration::from_secs(2));
    app.chrome_dirty = true;
}

/// Run a fresh search for `query` against the focused PDF buffer and
/// jump the focused window to the initial match (if any). Clears the
/// encoded-frame cache so the next paint composes the new highlights.
fn run_search(
    ws: &mut Workspace,
    app: &mut AppState,
    ecache: &Arc<ComposedEncodedCache>,
    query: &str,
    forward: bool,
) -> Result<()> {
    let buf_id = ws.focused_window().buffer;
    let from_page = ws.focused_window().viewport.page_idx;
    let (matches_count, target) = {
        let Some(buf) = ws.buffer_pdf_mut(buf_id) else {
            return Ok(());
        };
        buf.run_search(query, from_page, forward);
        let target = buf.search.current.and_then(|i| buf.search.matches.get(i).copied());
        (buf.search.matches.len(), target)
    };
    ecache.clear();
    if let Some(m) = target {
        let (x_off, y_off) = match_to_offsets(ws, buf_id, &m)?;
        ws.jump_to_page(m.page_idx, x_off, y_off)?;
    } else {
        ws.focused_window_mut().dirty = true;
    }
    if matches_count == 0 {
        set_message(app, format!("no match for {:?}", query));
    } else {
        let cur = ws
            .buffer_pdf(buf_id)
            .and_then(|b| b.search.current)
            .map(|i| i + 1)
            .unwrap_or(0);
        set_message(
            app,
            format!("/{}  [{}/{}]", query, cur, matches_count),
        );
    }
    Ok(())
}

/// `n` / `N` after a successful search. Steps to the next/previous
/// match and jumps the focused window there.
fn step_search(
    ws: &mut Workspace,
    app: &mut AppState,
    ecache: &Arc<ComposedEncodedCache>,
    forward: bool,
) -> Result<()> {
    let buf_id = ws.focused_window().buffer;
    let (target, total, current) = {
        let Some(buf) = ws.buffer_pdf_mut(buf_id) else {
            return Ok(());
        };
        let target = buf.step_search(forward);
        (target, buf.search.matches.len(), buf.search.current)
    };
    ecache.clear();
    let Some(m) = target else {
        set_message(app, "no matches".into());
        return Ok(());
    };
    let (x_off, y_off) = match_to_offsets(ws, buf_id, &m)?;
    ws.jump_to_page(m.page_idx, x_off, y_off)?;
    let cur = current.map(|i| i + 1).unwrap_or(0);
    set_message(app, format!("[{}/{}]", cur, total));
    Ok(())
}

/// Compute viewport `(x_off, y_off)` that scrolls the focused window
/// so the given match sits roughly centred. Reuses the focused
/// window's current zoom/rotation. Returns `(0, 0)` for the trivial
/// "fits on screen" case so we don't fight `clamp_offsets` for tiny
/// pages.
fn match_to_offsets(
    ws: &Workspace,
    buf_id: svreader_core::BufferId,
    m: &svreader_core::MatchRect,
) -> Result<(i32, i32)> {
    let Some(buf) = ws.buffer_pdf(buf_id) else {
        return Ok((0, 0));
    };
    let win = ws.focused_window();
    let page_size = buf.pdf.page_size(m.page_idx)?;
    let scale = win.viewport.display_scale(page_size);
    if scale <= 0.0 {
        return Ok((0, 0));
    }
    // Map the rect into rotated-page PDF points.
    let rotated = win.viewport.rotation.apply_to_size(page_size);
    use svreader_core::Rotation;
    let (rx0, ry0) = match win.viewport.rotation {
        Rotation::R0 => (m.rect.x0, m.rect.y0),
        Rotation::R90 => (page_size.height - m.rect.y1, m.rect.x0),
        Rotation::R180 => (page_size.width - m.rect.x1, page_size.height - m.rect.y1),
        Rotation::R270 => (m.rect.y0, page_size.width - m.rect.x1),
    };
    let cx = ((rx0) * scale).round() as i32;
    let cy = ((ry0) * scale).round() as i32;
    let sw = win.viewport.screen_w as i32;
    let sh = win.viewport.screen_h as i32;
    // Try to centre the match on screen.
    let target_x = cx - sw / 3;
    let target_y = cy - sh / 3;
    // Clamp offsets into valid scroll range for the rotated page.
    let pw = (rotated.width * scale).round().max(1.0) as u32;
    let ph = (rotated.height * scale).round().max(1.0) as u32;
    let (xmin, xmax) = win.viewport.x_range(pw);
    let (ymin, ymax) = win.viewport.y_range(ph);
    let x_off = target_x.clamp(xmin, xmax);
    let y_off = target_y.clamp(ymin, ymax);
    Ok((x_off, y_off))
}

/// Handle a key while the search prompt is open (after `/` or `?`).
fn handle_search_key(
    ws: &mut Workspace,
    ecache: &Arc<ComposedEncodedCache>,
    app: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    let Mode::Search { input, forward } = &mut app.mode else {
        return Ok(());
    };
    match k.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.full_repaint = true;
        }
        KeyCode::Enter => {
            let query = std::mem::take(input);
            let forward = *forward;
            app.mode = Mode::Normal;
            app.full_repaint = true;
            if !query.is_empty() {
                if let Err(e) = run_search(ws, app, ecache, &query, forward) {
                    set_message(app, format!("search: {e}"));
                }
            }
        }
        KeyCode::Backspace => {
            input.pop();
        }
        KeyCode::Char(c) => {
            if k.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                app.mode = Mode::Normal;
                app.full_repaint = true;
            } else {
                input.push(c);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Draw the `/<query>` or `?<query>` prompt at `row`. Lives on the
/// row right above the global status bar — same slot the `:` palette
/// uses, but with a single line and no completion column.
fn draw_search_prompt(
    stdout: &mut impl Write,
    row: u16,
    cols: u16,
    input: &str,
    forward: bool,
) -> Result<()> {
    let prefix = if forward { '/' } else { '?' };
    let line = format!("{}{}", prefix, input);
    let truncated: String = line.chars().take(cols as usize).collect();
    let pad = (cols as usize).saturating_sub(truncated.chars().count());
    write!(
        stdout,
        "\x1b[{};1H\x1b[2K{}{}",
        row + 1,
        truncated,
        " ".repeat(pad)
    )?;
    Ok(())
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
            | "Explore"
            | "Ex"
            | "Sexplore"
            | "Sex"
            | "Vexplore"
            | "Vex"
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

// ===================== M2: TOC / Marks / Mouse =====================

fn enter_toc_mode(ws: &mut Workspace, app: &mut AppState) -> Result<()> {
    let buf_id = ws.focused_window().buffer;
    let Some(pdf_buf) = ws.buffer_pdf(buf_id) else {
        return Err(anyhow::anyhow!("not a PDF buffer"));
    };
    let outline = pdf_buf.pdf.outline().unwrap_or_default();
    let mut entries: Vec<TocEntry> = Vec::new();
    flatten_outline(&outline, 0, &mut entries);
    if entries.is_empty() {
        return Err(anyhow::anyhow!("document has no outline"));
    }
    let cur_page = ws.focused_window().viewport.page_idx;
    let selected = entries
        .iter()
        .rposition(|e| e.page <= cur_page)
        .unwrap_or(0);
    app.mode = Mode::Toc {
        entries,
        selected,
        scroll: 0,
        pending: KeyParserState::default(),
    };
    // Deliberately NOT setting `full_repaint = true`: the overlay's
    // own row-wipe clears the sixels behind it, and forcing a full
    // re-emit on every j/k causes a visible flash. Same pattern as
    // Mode::Help — only the close path repaints.
    Ok(())
}

fn enter_marks_mode(ws: &mut Workspace, app: &mut AppState) -> Result<()> {
    let buf_id = ws.focused_window().buffer;
    let Some(pdf_buf) = ws.buffer_pdf(buf_id) else {
        return Err(anyhow::anyhow!("not a PDF buffer"));
    };
    let mut entries: Vec<MarkEntry> = pdf_buf
        .state
        .bookmarks
        .iter()
        .map(|b| MarkEntry {
            mark: b.mark,
            page: b.page,
            x_off: b.x_off,
            y_off: b.y_off,
        })
        .collect();
    entries.sort_by_key(|e| e.mark);
    if entries.is_empty() {
        return Err(anyhow::anyhow!("no marks set (use m{{a-z}} to set one)"));
    }
    app.mode = Mode::Marks {
        entries,
        selected: 0,
        scroll: 0,
        pending: KeyParserState::default(),
    };
    // See `enter_toc_mode` — overlay clears its own rect; a forced
    // full repaint here would just re-emit the sixels behind it.
    Ok(())
}

fn flatten_outline(items: &[Outline], depth: usize, out: &mut Vec<TocEntry>) {
    for item in items {
        if let Some(p) = item.page {
            out.push(TocEntry {
                depth,
                title: item.title.clone(),
                page: p,
            });
        } else {
            // Section header without a page link — still useful as a
            // visual landmark, point it at page 0 (the Enter handler
            // will just do nothing useful, but j/k still works past
            // it).
            out.push(TocEntry {
                depth,
                title: item.title.clone(),
                page: 0,
            });
        }
        flatten_outline(&item.children, depth + 1, out);
    }
}

/// Shared move-by-N logic for TOC + Marks. Counts come from the
/// per-mode `KeyParserState` so `5j` works in both lists.
fn list_move(selected: &mut usize, total: usize, delta: isize) {
    if total == 0 {
        return;
    }
    let cur = *selected as isize;
    let mut next = cur + delta;
    if next < 0 {
        next = 0;
    }
    if next >= total as isize {
        next = total as isize - 1;
    }
    *selected = next as usize;
}

fn handle_toc_key(ws: &mut Workspace, app: &mut AppState, k: KeyEvent) -> Result<()> {
    let Mode::Toc {
        entries,
        selected,
        scroll: _,
        pending,
    } = &mut app.mode
    else {
        return Ok(());
    };
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t')) && !pending.active() {
        app.mode = Mode::Normal;
        app.full_repaint = true;
        return Ok(());
    }
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.mode = Mode::Normal;
        app.full_repaint = true;
        return Ok(());
    }
    if k.code == KeyCode::Enter {
        let target = entries[*selected].page;
        // Snapshot mode entries before we move out of the borrow.
        let target_page = target;
        app.mode = Mode::Normal;
        app.full_repaint = true;
        return ws.jump_to_page(target_page, 0, 0);
    }
    let total = entries.len();
    let count = pending.count.unwrap_or(1).max(1) as isize;
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => {
            list_move(selected, total, count);
            pending.clear();
        }
        KeyCode::Char('k') | KeyCode::Up => {
            list_move(selected, total, -count);
            pending.clear();
        }
        KeyCode::Char('G') | KeyCode::End => {
            *selected = match pending.count.take() {
                Some(n) => (n.saturating_sub(1)).min(total.saturating_sub(1)),
                None => total.saturating_sub(1),
            };
            pending.clear();
        }
        KeyCode::Home => {
            *selected = 0;
            pending.clear();
        }
        KeyCode::Char(c) if c.is_ascii_digit() && (c != '0' || pending.count.is_some()) => {
            let d = (c as u8 - b'0') as usize;
            pending.count = Some(pending.count.unwrap_or(0).saturating_mul(10).saturating_add(d));
        }
        KeyCode::Char('g') => {
            if pending.leader == svreader_core::keys::Leader::G {
                *selected = pending.count.take().unwrap_or(1).saturating_sub(1).min(total.saturating_sub(1));
                pending.clear();
            } else {
                pending.leader = svreader_core::keys::Leader::G;
            }
        }
        _ => {
            pending.clear();
        }
    }
    // No full_repaint: only the overlay needs to redraw, and it
    // does that unconditionally each loop iteration in TOC mode.
    Ok(())
}

fn handle_marks_key(ws: &mut Workspace, app: &mut AppState, k: KeyEvent) -> Result<()> {
    let Mode::Marks {
        entries,
        selected,
        scroll: _,
        pending,
    } = &mut app.mode
    else {
        return Ok(());
    };
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) && !pending.active() {
        app.mode = Mode::Normal;
        app.full_repaint = true;
        return Ok(());
    }
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.mode = Mode::Normal;
        app.full_repaint = true;
        return Ok(());
    }
    if k.code == KeyCode::Enter {
        let target = entries[*selected];
        app.mode = Mode::Normal;
        app.full_repaint = true;
        return ws.jump_to_page(target.page, target.x_off, target.y_off);
    }
    if k.code == KeyCode::Char('d') {
        // `d` deletes the selected mark, then refreshes the list.
        let mark = entries[*selected].mark;
        if ws.delete_bookmark(mark) {
            entries.remove(*selected);
            if *selected >= entries.len() && *selected > 0 {
                *selected -= 1;
            }
            if entries.is_empty() {
                app.mode = Mode::Normal;
                // List now empty → falling back to Normal: need a
                // full repaint to bring the page back.
                app.full_repaint = true;
            }
        }
        return Ok(());
    }
    let total = entries.len();
    let count = pending.count.unwrap_or(1).max(1) as isize;
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => {
            list_move(selected, total, count);
            pending.clear();
        }
        KeyCode::Char('k') | KeyCode::Up => {
            list_move(selected, total, -count);
            pending.clear();
        }
        KeyCode::Char('G') | KeyCode::End => {
            *selected = total.saturating_sub(1);
            pending.clear();
        }
        KeyCode::Home => {
            *selected = 0;
            pending.clear();
        }
        KeyCode::Char(c) if c.is_ascii_digit() && (c != '0' || pending.count.is_some()) => {
            let d = (c as u8 - b'0') as usize;
            pending.count = Some(pending.count.unwrap_or(0).saturating_mul(10).saturating_add(d));
        }
        KeyCode::Char('g') => {
            if pending.leader == svreader_core::keys::Leader::G {
                *selected = 0;
                pending.clear();
            } else {
                pending.leader = svreader_core::keys::Leader::G;
            }
        }
        _ => {
            pending.clear();
        }
    }
    // No full_repaint here either — the overlay redraw is enough.
    Ok(())
}

fn draw_toc_overlay(
    stdout: &mut impl Write,
    body: CellRect,
    entries: &[TocEntry],
    selected: usize,
    _scroll: usize,
) -> Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    let cols = body.cols as usize;
    // Blank the body rect first.
    for r in 0..body.rows {
        write!(stdout, "\x1b[{};{}H\x1b[2K", body.row + r + 1, body.col + 1)?;
    }
    let header = format!(" TOC — {} entries — Enter open · j/k move · gg/G first/last · q close", entries.len());
    let header_t: String = header.chars().take(cols).collect();
    let pad = cols.saturating_sub(header_t.chars().count());
    write!(
        stdout,
        "\x1b[{};{}H\x1b[48;5;236m\x1b[38;5;252m{}{}\x1b[0m",
        body.row + 1,
        body.col + 1,
        header_t,
        " ".repeat(pad)
    )?;

    let list_rows = body.rows.saturating_sub(1) as usize;
    if list_rows == 0 {
        return Ok(());
    }
    let scroll = if selected >= list_rows {
        selected + 1 - list_rows
    } else {
        0
    };
    let visible = list_rows.min(entries.len().saturating_sub(scroll));
    for i in 0..visible {
        let abs = scroll + i;
        let Some(entry) = entries.get(abs) else { break };
        let row = body.row + 1 + i as u16;
        let indent = "  ".repeat(entry.depth.min(8));
        let line = format!(
            "{}{}  · p{}",
            indent,
            entry.title,
            entry.page.saturating_add(1)
        );
        let truncated: String = line.chars().take(cols).collect();
        let pad = cols.saturating_sub(truncated.chars().count());
        let selected_here = abs == selected;
        write!(stdout, "\x1b[{};{}H", row + 1, body.col + 1)?;
        if selected_here {
            write!(stdout, "\x1b[7m")?;
        }
        write!(stdout, "{}{}", truncated, " ".repeat(pad))?;
        if selected_here {
            write!(stdout, "\x1b[0m")?;
        }
    }
    Ok(())
}

fn draw_marks_overlay(
    stdout: &mut impl Write,
    body: CellRect,
    entries: &[MarkEntry],
    selected: usize,
    _scroll: usize,
) -> Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    let cols = body.cols as usize;
    for r in 0..body.rows {
        write!(stdout, "\x1b[{};{}H\x1b[2K", body.row + r + 1, body.col + 1)?;
    }
    let header = format!(
        " marks ({}) — Enter jump · d delete · j/k move · q close",
        entries.len()
    );
    let header_t: String = header.chars().take(cols).collect();
    let pad = cols.saturating_sub(header_t.chars().count());
    write!(
        stdout,
        "\x1b[{};{}H\x1b[48;5;236m\x1b[38;5;252m{}{}\x1b[0m",
        body.row + 1,
        body.col + 1,
        header_t,
        " ".repeat(pad)
    )?;
    let list_rows = body.rows.saturating_sub(1) as usize;
    if list_rows == 0 {
        return Ok(());
    }
    let scroll = if selected >= list_rows {
        selected + 1 - list_rows
    } else {
        0
    };
    let visible = list_rows.min(entries.len().saturating_sub(scroll));
    for i in 0..visible {
        let abs = scroll + i;
        let Some(entry) = entries.get(abs) else { break };
        let row = body.row + 1 + i as u16;
        let line = format!(
            "  '{}    page {}    ({},{})",
            entry.mark,
            entry.page.saturating_add(1),
            entry.x_off,
            entry.y_off
        );
        let truncated: String = line.chars().take(cols).collect();
        let pad = cols.saturating_sub(truncated.chars().count());
        let selected_here = abs == selected;
        write!(stdout, "\x1b[{};{}H", row + 1, body.col + 1)?;
        if selected_here {
            write!(stdout, "\x1b[7m")?;
        }
        write!(stdout, "{}{}", truncated, " ".repeat(pad))?;
        if selected_here {
            write!(stdout, "\x1b[0m")?;
        }
    }
    Ok(())
}

fn handle_mouse(
    ws: &mut Workspace,
    app: &mut AppState,
    m: MouseEvent,
    geom: TermGeom,
) -> Result<()> {
    // Only act on left-click down. Up / drag / move / scroll are
    // ignored; scroll could navigate later but isn't part of M2.
    if !matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        return Ok(());
    }
    let tab_bar_rows: u16 = if ws.tab_count() > 1 { 1 } else { 0 };
    let body = body_rect(geom, tab_bar_rows);
    let layout = ws.layout(body);
    // Find the window the click is in.
    let Some((win_id, rect)) = layout.iter().find(|(_, r)| {
        m.column >= r.col
            && m.column < r.col.saturating_add(r.cols)
            && m.row >= r.row
            && m.row < r.row.saturating_add(r.rows)
    }) else {
        return Ok(());
    };
    let win_id = *win_id;
    let rect = *rect;
    // If the click is in a different window than the focused one,
    // refocus first (vim's mouse-click behaviour). Repaints handled
    // by the main loop.
    if win_id != ws.current_tab().focused {
        ws.set_focus_window(win_id);
        app.full_repaint = true;
        return Ok(());
    }
    // Convert cell to pixel within the window.
    let local_col = m.column - rect.col;
    let local_row = m.row - rect.row;
    let px_x = local_col as i32 * geom.cell_px_w as i32 + (geom.cell_px_w as i32 / 2);
    let px_y = local_row as i32 * geom.cell_px_h as i32 + (geom.cell_px_h as i32 / 2);
    if let Err(e) = ws.click_at(win_id, px_x, px_y) {
        set_message(app, format!("link: {e}"));
    } else {
        // A successful link follow triggers a full repaint.
        app.full_repaint = true;
    }
    Ok(())
}

fn set_mouse_capture(app: &mut AppState, on: bool) {
    if app.mouse_enabled == on {
        return;
    }
    app.mouse_enabled = on;
    let mut stdout = io::stdout();
    if on {
        let _ = execute!(stdout, EnableMouseCapture);
    } else {
        let _ = execute!(stdout, DisableMouseCapture);
    }
}

/// Extract every page's text from the focused PDF buffer, write it
/// to a temp file, suspend our raw-mode terminal, and hand control
/// over to `$EDITOR` (defaulting to `vi`/`vim`/`nano` in that order).
/// Restores our screen on return.
fn open_document_text_in_editor(ws: &Workspace) -> Result<()> {
    use anyhow::anyhow;
    use std::process::Command as StdCommand;

    let buf_id = ws.focused_window().buffer;
    let Some(buf) = ws.buffer_pdf(buf_id) else {
        return Err(anyhow!("not a PDF buffer"));
    };
    let n = buf.pdf.page_count();
    let mut text = String::new();
    for page_idx in 0..n {
        if page_idx > 0 {
            text.push_str("\n\n");
        }
        text.push_str(&format!("--- Page {} ---\n\n", page_idx + 1));
        match buf.pdf.page_text(page_idx) {
            Ok(t) => text.push_str(&t),
            Err(e) => text.push_str(&format!("[failed to extract page {}: {}]\n", page_idx + 1, e)),
        }
    }

    // Write to a temp file alongside `/tmp` so the editor opens a
    // real path. Filename includes the document stem so `:f` /
    // `Ctrl-G` shows something recognisable.
    let stem = buf
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".into());
    let pid = std::process::id();
    let tmp_path = std::env::temp_dir().join(format!("svreader-{stem}-{pid}.txt"));
    std::fs::write(&tmp_path, &text)
        .with_context(|| format!("writing {:?}", tmp_path))?;

    // Pick an editor.
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| {
            for cand in ["vim", "vi", "nano"] {
                if which(cand).is_some() {
                    return cand.to_string();
                }
            }
            "vi".to_string()
        });

    // Hand the terminal to the editor: leave alt-screen, drop raw
    // mode, hide our cursor management, restore mouse-off so the
    // editor sees normal input.
    let mut stdout = io::stdout();
    let _ = execute!(stdout, DisableFocusChange);
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();

    // Run editor — let it inherit our std{in,out,err} fully.
    let status_res = StdCommand::new(&editor).arg(&tmp_path).status();

    // Reclaim the terminal.
    let _ = enable_raw_mode();
    let mut stdout2 = io::stdout();
    let _ = execute!(stdout2, EnterAlternateScreen, cursor::Hide, EnableFocusChange);
    // Mouse capture is re-enabled by the outer event-loop's full
    // repaint pass via set_mouse_capture's invariants — the
    // reader's `app.mouse_enabled` flag still says what we want.
    // Force a clear so any leftover paint from the editor doesn't
    // mix with our sixel image.
    let _ = write!(stdout2, "\x1b[2J");

    match status_res {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(anyhow!("editor exited with {}", s)),
        Err(e) => Err(anyhow!("failed to run {editor:?}: {e}")),
    }
}

/// Like `which(1)` but minimal: walks `$PATH` and returns the first
/// match. Used as a fallback when neither `$EDITOR` nor `$VISUAL` is
/// set so `:text` still does something sensible on a fresh shell.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Render the focused window's current page and push the PNG bytes to
/// the system clipboard. Uses the full rasterised page (not the
/// scroll-cropped view and without night-mode inversion) so the
/// clipboard receives a clean copy of the PDF page at the current
/// zoom/rotation. Returns the name of the clipboard tool that worked.
fn copy_current_page_to_clipboard(ws: &Workspace) -> Result<&'static str> {
    use anyhow::anyhow;
    let win = ws.focused_window();
    let Some(buf) = ws.buffer_pdf(win.buffer) else {
        return Err(anyhow!("not a PDF buffer"));
    };
    let (page, _) = Renderer::render_page(&buf.pdf, &win.viewport)?;
    let mut png = Vec::new();
    {
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                page.image.as_raw(),
                page.image.width(),
                page.image.height(),
                image::ExtendedColorType::Rgba8,
            )
            .context("png encode failed")?;
    }
    pipe_to_clipboard(&png)
}

/// Try the available clipboard tools in order and pipe `data` to the
/// first one that works. On Wayland prefer `wl-copy`; fall back to
/// X11 tools. Returns the name of the tool that succeeded.
fn pipe_to_clipboard(data: &[u8]) -> Result<&'static str> {
    use std::process::{Command as StdCommand, Stdio};
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut tools: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    if wayland {
        tools.push(("wl-copy", vec!["--type", "image/png"]));
        tools.push(("xclip", vec!["-selection", "clipboard", "-t", "image/png"]));
    } else {
        tools.push(("xclip", vec!["-selection", "clipboard", "-t", "image/png"]));
        tools.push(("wl-copy", vec!["--type", "image/png"]));
    }

    let mut last_err: Option<String> = None;
    for (name, args) in tools {
        let spawn = StdCommand::new(name)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawn {
            Ok(c) => c,
            Err(e) => {
                last_err = Some(format!("{name}: {e}"));
                continue;
            }
        };
        if let Some(stdin) = child.stdin.as_mut() {
            if let Err(e) = stdin.write_all(data) {
                last_err = Some(format!("{name}: write: {e}"));
                let _ = child.kill();
                continue;
            }
        }
        // Drop stdin to close the pipe, then wait.
        drop(child.stdin.take());
        match child.wait() {
            Ok(status) if status.success() => return Ok(name),
            Ok(status) => {
                last_err = Some(format!("{name}: exit {status}"));
            }
            Err(e) => {
                last_err = Some(format!("{name}: wait: {e}"));
            }
        }
    }
    Err(anyhow::anyhow!(
        "no clipboard tool worked (install wl-clipboard or xclip){}",
        last_err
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    ))
}

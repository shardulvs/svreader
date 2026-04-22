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
use svreader_core::keys::{Key, KeyOutcome, KeyParser, KeyParserState};
use svreader_core::{
    Action, ColorPalette, CommandRegistry, Document, DocState, Navigator, PageCache,
    ParsedCommand, PdfDocument, PrefetchRequest, Prefetcher, Renderer, Rotation, Viewport,
    ZoomMode,
};

use crate::capabilities::{probe_sixel, SIXEL_TERMINALS};
use crate::sixel_write::{blank_rows, encode_and_write, ColorMode};
use crate::terminal::{self, TermGeom};
use crate::timings::{FrameTiming, TimingsLog};
use crate::RunOptions;

const STATUS_ROWS: u16 = 1;
/// Maximum rows the command palette can expand into.
const PALETTE_MAX_ROWS: u16 = 6;
/// Rows the help overlay occupies.
const HELP_ROWS: u16 = 20;

#[derive(Debug, Clone)]
enum Mode {
    Normal,
    Command {
        input: String,
        cursor: usize,
        completion_idx: Option<usize>,
    },
    Help,
}

struct AppState {
    viewport: Viewport,
    last_timing: Option<FrameTiming>,
    /// Effective DPI of the last rendered frame. Kept so status-only
    /// repaints don't have to recompute from a page size we'd have
    /// to re-fetch.
    last_dpi: f32,
    pending_hint: String,
    message: Option<String>,
    message_expires: Option<Instant>,
    key_state: KeyParserState,
    mode: Mode,
    last_sixel_rows: u16,
    prefetch_radius: usize,
    color_mode: ColorMode,
    dirty: bool,
    needs_status: bool,
    quit: bool,
}

pub fn run(opts: RunOptions) -> Result<()> {
    let pdf_path = opts.pdf.clone();
    let doc = PdfDocument::open(&pdf_path)
        .with_context(|| format!("opening {:?}", pdf_path))?;
    if doc.page_count() == 0 {
        anyhow::bail!("PDF has no pages");
    }

    // Sidecar state
    let mut doc_state = DocState::load(&pdf_path).unwrap_or_default();
    if let Some(start) = opts.start_page {
        doc_state.last_page = start.saturating_sub(1);
    }

    enable_raw_mode().context("enable_raw_mode failed")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)
        .context("alt-screen enter failed")?;

    let res = (|| -> Result<()> {
        // Draw a loading banner immediately so the user sees *something*
        // before the first sixel lands. Useful when the first mupdf
        // render is slow, or when sixel gets dropped by tmux.
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

        let (img_w, img_h) = geom.image_px_for_rows(STATUS_ROWS);
        let mut viewport = Viewport {
            page_idx: doc_state.last_page.min(doc.page_count().saturating_sub(1)),
            x_off: doc_state.scroll_x,
            y_off: doc_state.scroll_y,
            zoom: doc_state.zoom,
            rotation: doc_state.rotation,
            screen_w: img_w,
            screen_h: img_h,
            night_mode: doc_state.night_mode,
            render_dpi: doc_state.render_dpi,
            render_quality: doc_state.render_quality,
        };
        ensure_valid_offsets(&doc, &mut viewport)?;

        let cache = Arc::new(PageCache::new(5));
        cache.set_enabled(doc_state.cache_enabled);
        let prefetcher = Prefetcher::spawn(&doc, cache.clone())?;

        let log_path = std::env::var("SVREADER_TIMINGS_LOG")
            .ok()
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from("/tmp/svreader-timings.log")));
        let timings_log = TimingsLog::open(log_path);
        let registry = CommandRegistry::default();

        let mut app = AppState {
            viewport,
            last_timing: None,
            last_dpi: 72.0,
            pending_hint: String::new(),
            message: None,
            message_expires: None,
            key_state: KeyParserState::default(),
            mode: Mode::Normal,
            last_sixel_rows: 0,
            prefetch_radius: 2,
            color_mode: ColorMode::XTerm256,
            dirty: true,
            needs_status: true,
            quit: false,
        };

        write!(stdout, "\x1b[2J")?;
        stdout.flush()?;

        let file_name = pdf_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let page_count = doc.page_count();

        while !app.quit {
            if let Some(t) = app.message_expires {
                if Instant::now() >= t {
                    app.message = None;
                    app.message_expires = None;
                    app.needs_status = true;
                }
            }

            if app.dirty {
                draw_frame(
                    &doc,
                    &cache,
                    &mut app,
                    geom,
                    &mut stdout,
                    &file_name,
                    page_count,
                    &timings_log,
                )?;
                app.dirty = false;
                app.needs_status = false;
                fire_prefetches(&doc, &app, &cache, &prefetcher);
            } else if app.needs_status {
                draw_status(
                    &app,
                    geom,
                    &mut stdout,
                    &file_name,
                    page_count,
                    cache.stats(),
                )?;
                app.needs_status = false;
            }

            match &app.mode {
                Mode::Normal => {}
                Mode::Command {
                    input,
                    cursor: _,
                    completion_idx,
                } => {
                    let completions: Vec<(String, bool)> = registry
                        .complete(input)
                        .into_iter()
                        .map(|c| (format!(":{}  — {}", c.name, c.description), false))
                        .collect();
                    let bottom = geom.rows.saturating_sub(STATUS_ROWS);
                    let top = bottom.saturating_sub(PALETTE_MAX_ROWS);
                    draw_palette(
                        &mut stdout,
                        top,
                        bottom,
                        geom.cols,
                        input,
                        &completions,
                        *completion_idx,
                    )?;
                    let input_row = bottom.saturating_sub(1);
                    let col = (input.chars().count() as u16).saturating_add(2);
                    write!(stdout, "\x1b[{};{}H", input_row + 1, col)?;
                    execute!(stdout, cursor::Show)?;
                }
                Mode::Help => {
                    let bottom = geom.rows.saturating_sub(STATUS_ROWS);
                    let top = bottom.saturating_sub(HELP_ROWS);
                    draw_help(&mut stdout, top, bottom, geom.cols)?;
                }
            }
            stdout.flush()?;

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
                    let (w, h) = geom.image_px_for_rows(STATUS_ROWS);
                    Navigator::apply(&doc, &mut app.viewport, Action::Resize(w, h))?;
                    cache.clear();
                    app.last_sixel_rows = 0;
                    app.dirty = true;
                    write!(stdout, "\x1b[2J")?;
                }
                Event::Key(k) => {
                    if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        continue;
                    }
                    execute!(stdout, cursor::Hide)?;
                    match app.mode.clone() {
                        Mode::Normal => handle_normal_key(&doc, &mut app, k)?,
                        Mode::Command { .. } => {
                            handle_command_key(&doc, &cache, &mut app, &registry, k, &mut stdout)?;
                        }
                        Mode::Help => {
                            if matches!(k.code, KeyCode::Esc)
                                || k.code == KeyCode::Char('?')
                                || k.code == KeyCode::Char('q')
                            {
                                app.mode = Mode::Normal;
                                write!(stdout, "\x1b[2J")?;
                                app.dirty = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        doc_state.last_page = app.viewport.page_idx;
        doc_state.zoom = app.viewport.zoom;
        doc_state.rotation = app.viewport.rotation;
        doc_state.scroll_x = app.viewport.x_off;
        doc_state.scroll_y = app.viewport.y_off;
        doc_state.night_mode = app.viewport.night_mode;
        doc_state.render_dpi = app.viewport.render_dpi;
        doc_state.render_quality = app.viewport.render_quality;
        doc_state.cache_enabled = cache.enabled();
        if let Err(e) = doc_state.save(&pdf_path) {
            tracing::warn!("failed to save sidecar: {e:#}");
        }

        drop(prefetcher);
        Ok(())
    })();

    let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
    res
}

fn ensure_valid_offsets<D: Document>(doc: &D, viewport: &mut Viewport) -> Result<()> {
    if doc.page_count() == 0 {
        return Ok(());
    }
    let page_size = doc.page_size(viewport.page_idx.min(doc.page_count() - 1))?;
    viewport.clamp_offsets(page_size);
    Ok(())
}

fn draw_frame<D: Document>(
    doc: &D,
    cache: &Arc<PageCache>,
    app: &mut AppState,
    geom: TermGeom,
    stdout: &mut impl Write,
    file_name: &str,
    page_count: usize,
    timings_log: &TimingsLog,
) -> Result<()> {
    let page_size = doc.page_size(app.viewport.page_idx)?;
    let display_scale = app.viewport.display_scale(page_size);
    let raster_scale = app.viewport.raster_scale(page_size);
    let key = CacheKey::new(
        app.viewport.page_idx,
        display_scale,
        raster_scale,
        app.viewport.rotation,
    );

    let t_overall = Instant::now();
    let (page, render_dur) = if let Some(hit) = cache.get(&key) {
        (hit, Duration::ZERO)
    } else {
        let (page, rt) = Renderer::render_page(doc, &app.viewport)?;
        let arc: Arc<CachedPage> = Arc::new(page);
        cache.insert(key, arc.clone());
        (arc, rt.render)
    };
    let (composed, compose) = Renderer::compose(&page, &app.viewport);

    let image_rows = (composed.height() as u32).div_ceil(geom.cell_px_h as u32) as u16;
    if app.last_sixel_rows > image_rows {
        blank_rows(image_rows, app.last_sixel_rows - image_rows, geom.cols, stdout).ok();
    }
    let emit = encode_and_write(&composed, 0, 0, app.color_mode, stdout)?;
    app.last_sixel_rows = image_rows;

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
    timings_log.record(app.viewport.page_idx, &timing);
    app.last_timing = Some(timing);
    app.last_dpi = app.viewport.effective_dpi(page_size);

    draw_status(app, geom, stdout, file_name, page_count, cache.stats())?;

    Ok(())
}

fn draw_status(
    app: &AppState,
    geom: TermGeom,
    stdout: &mut impl Write,
    file_name: &str,
    page_count: usize,
    cache_stats: (usize, usize),
) -> Result<()> {
    let row = geom.rows.saturating_sub(STATUS_ROWS);
    let v = &app.viewport;
    let mut s = String::new();
    s.push_str(&format!(
        " {} | {}/{} | {} | {}\u{00B0}",
        file_name,
        v.page_idx + 1,
        page_count.max(1),
        v.zoom.label(),
        v.rotation.degrees(),
    ));
    if v.night_mode {
        s.push_str(" [night]");
    }
    s.push_str(&format!(
        " dpi:{}{}",
        app.last_dpi.round() as i32,
        if v.render_dpi.is_some() { "*" } else { "" }
    ));
    if (v.render_quality - 1.0).abs() > 0.005 {
        s.push_str(&format!(" q:{}%", (v.render_quality * 100.0).round() as i32));
    }
    s.push_str(&format!(" cache:{}/{}", cache_stats.0, cache_stats.1));
    if let Some(t) = &app.last_timing {
        s.push(' ');
        s.push_str(&t.short_label());
    }
    if !app.pending_hint.is_empty() {
        s.push_str(&format!(" [{}]", app.pending_hint));
    }
    if let Some(msg) = &app.message {
        s.push_str(&format!(" -- {}", msg));
    }
    // Truncate to fit.
    let truncated: String = s.chars().take(geom.cols as usize).collect();
    let pad = (geom.cols as usize).saturating_sub(truncated.chars().count());
    // Dark background + light text. 256-colour codes (236 / 252) for a
    // softer grey than pure black/white; every sixel-capable terminal
    // speaks 256 colour so this is safe.
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
    completions: &[(String, bool)],
    cursor_idx: Option<usize>,
) -> Result<()> {
    for r in top..bottom {
        write!(stdout, "\x1b[{};1H\x1b[2K", r + 1)?;
    }
    let max_comp = (bottom - top).saturating_sub(1) as usize;
    for (i, (c, _)) in completions.iter().take(max_comp).enumerate() {
        let row = bottom.saturating_sub(2 + i as u16);
        write!(stdout, "\x1b[{};1H", row + 1)?;
        let selected = Some(i) == cursor_idx;
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
    let input_row = bottom.saturating_sub(1);
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
        "   gg / G        first / last page   Ng or :N goto page",
        "   H M L         page top / middle / bottom",
        "   h / l         scroll left / right",
        "   w / e / f     fit width / height / page",
        "   + / -         zoom in / out",
        "   r / R         rotate CW / CCW",
        "   n             toggle night mode",
        "   :             command palette",
        "   ?             toggle this help",
        "   q             quit",
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

fn fire_prefetches<D: Document>(
    doc: &D,
    app: &AppState,
    cache: &Arc<PageCache>,
    prefetcher: &Prefetcher,
) {
    if !cache.enabled() || app.prefetch_radius == 0 {
        return;
    }
    let n = app.prefetch_radius;
    let count = doc.page_count();
    if count == 0 {
        return;
    }
    let start = app.viewport.page_idx.saturating_sub(n);
    let end = (app.viewport.page_idx + n).min(count - 1);
    for idx in start..=end {
        if idx == app.viewport.page_idx {
            continue;
        }
        let mut vp = app.viewport.clone();
        vp.page_idx = idx;
        vp.x_off = 0;
        vp.y_off = 0;
        let ps = match doc.page_size(idx) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let key = CacheKey::new(idx, vp.display_scale(ps), vp.raster_scale(ps), vp.rotation);
        if cache.contains(&key) {
            continue;
        }
        prefetcher.request(PrefetchRequest { viewport: vp, key });
    }
}

fn handle_normal_key<D: Document>(doc: &D, app: &mut AppState, k: KeyEvent) -> Result<()> {
    // Ctrl-C always exits.
    if matches!(k.code, KeyCode::Char('c')) && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return Ok(());
    }
    let Some(key) = crossterm_to_key(k) else {
        return Ok(());
    };
    let outcome = KeyParser::feed(&mut app.key_state, key);
    app.pending_hint = app.key_state.hint();
    match outcome {
        KeyOutcome::Pending => {
            app.needs_status = true;
        }
        KeyOutcome::OpenCommand => {
            app.mode = Mode::Command {
                input: String::new(),
                cursor: 0,
                completion_idx: None,
            };
        }
        KeyOutcome::ToggleHelp => {
            app.mode = Mode::Help;
        }
        KeyOutcome::Quit => {
            app.quit = true;
        }
        KeyOutcome::Action { action, count } => {
            for _ in 0..count {
                Navigator::apply(doc, &mut app.viewport, action.clone())?;
            }
            app.dirty = true;
        }
    }
    Ok(())
}

fn handle_command_key<D: Document>(
    doc: &D,
    cache: &Arc<PageCache>,
    app: &mut AppState,
    registry: &CommandRegistry,
    k: KeyEvent,
    stdout: &mut impl Write,
) -> Result<()> {
    let Mode::Command {
        input,
        cursor,
        completion_idx,
    } = &mut app.mode
    else {
        return Ok(());
    };
    match k.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            write!(stdout, "\x1b[2J")?;
            app.dirty = true;
        }
        KeyCode::Enter => {
            let line = std::mem::take(input);
            app.mode = Mode::Normal;
            write!(stdout, "\x1b[2J")?;
            app.dirty = true;
            if !line.is_empty() {
                match execute_command(doc, cache, app, registry, &line) {
                    Ok(()) => {}
                    Err(e) => {
                        app.message = Some(format!("{e}"));
                        app.message_expires = Some(Instant::now() + Duration::from_secs(3));
                    }
                }
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            let reverse = matches!(k.code, KeyCode::BackTab);
            let completions: Vec<String> = registry
                .complete(input)
                .into_iter()
                .map(|c| c.name.to_string())
                .collect();
            if !completions.is_empty() {
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
                *input = completions[idx].clone();
                *cursor = input.len();
            }
        }
        KeyCode::Backspace => {
            if !input.is_empty() {
                input.pop();
                *cursor = input.len();
                *completion_idx = None;
            }
        }
        KeyCode::Char(c) => {
            if k.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                app.mode = Mode::Normal;
                write!(stdout, "\x1b[2J")?;
                app.dirty = true;
            } else {
                input.push(c);
                *cursor = input.len();
                *completion_idx = None;
            }
        }
        _ => {}
    }
    Ok(())
}

fn execute_command<D: Document>(
    doc: &D,
    cache: &Arc<PageCache>,
    app: &mut AppState,
    registry: &CommandRegistry,
    line: &str,
) -> Result<()> {
    let parsed = registry.parse(line)?;
    match parsed {
        ParsedCommand::Nav(action) => {
            Navigator::apply(doc, &mut app.viewport, action)?;
            app.dirty = true;
        }
        ParsedCommand::Quit => {
            app.quit = true;
        }
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
            app.prefetch_radius = n;
            set_message(app, format!("prefetch {}", n));
        }
        ParsedCommand::Reset => {
            app.viewport.render_dpi = None;
            app.viewport.render_quality = 1.0;
            Navigator::apply(doc, &mut app.viewport, Action::SetRotation(Rotation::R0))?;
            Navigator::apply(doc, &mut app.viewport, Action::SetZoom(ZoomMode::FitWidth))?;
            cache.clear();
            app.dirty = true;
        }
        ParsedCommand::Colors(p) => {
            app.color_mode = match p {
                ColorPalette::XTerm256 => ColorMode::XTerm256,
                ColorPalette::Grayscale => ColorMode::Grayscale,
            };
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
            app.dirty = true;
        }
    }
    Ok(())
}

fn set_message(app: &mut AppState, msg: String) {
    app.message = Some(msg);
    app.message_expires = Some(Instant::now() + Duration::from_secs(2));
    app.needs_status = true;
}

fn crossterm_to_key(k: KeyEvent) -> Option<Key> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let key = match k.code {
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
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

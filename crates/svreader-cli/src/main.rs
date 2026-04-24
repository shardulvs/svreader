//! svreader-cli — debugging / test harness for `svreader-core`.
//!
//! Every rendering decision in the reader must be reproducible from
//! this CLI without needing a terminal.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use svreader_core::keys::{Key, KeyOutcome, KeyParser, KeyParserState};
use svreader_core::{
    Action, CommandRegistry, Document, Navigator, PageMetrics, ParsedCommand, PdfDocument,
    Renderer, Rotation, Viewport, ZoomMode,
};

#[derive(Parser, Debug)]
#[command(name = "svreader-cli", about = "Headless svreader debug harness")]
struct Cli {
    #[command(subcommand)]
    command: CliCmd,
}

#[derive(Subcommand, Debug)]
enum CliCmd {
    /// Print document info.
    Info { pdf: PathBuf },

    /// Render a single page to PNG.
    Render {
        pdf: PathBuf,
        #[arg(long, default_value = "1")]
        page: usize,
        #[arg(long, default_value = "fit-w")]
        zoom: String,
        #[arg(long, default_value = "0")]
        rotate: i32,
        #[arg(long, default_value = "1200x800")]
        screen: String,
        #[arg(long, default_value = "0")]
        x: i32,
        #[arg(long, default_value = "0")]
        y: i32,
        #[arg(long, default_value_t = false)]
        night: bool,
        #[arg(long)]
        dpi: Option<f32>,
        #[arg(long, default_value = "100")]
        quality: f32,
        #[arg(long)]
        out: PathBuf,
    },

    /// Replay keystrokes through the navigator and dump one PNG per frame.
    Simulate {
        pdf: PathBuf,
        #[arg(long)]
        keys: String,
        #[arg(long, default_value = "1200x800")]
        screen: String,
        #[arg(long)]
        out_dir: PathBuf,
    },

    /// Print PDF outline.
    Outline { pdf: PathBuf },

    /// Run a `:` command against the doc and report what it would do.
    Command {
        pdf: PathBuf,
        #[arg(long)]
        line: String,
    },
}

fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        CliCmd::Info { pdf } => cmd_info(pdf),
        CliCmd::Render {
            pdf,
            page,
            zoom,
            rotate,
            screen,
            x,
            y,
            night,
            dpi,
            quality,
            out,
        } => cmd_render(RenderArgs {
            pdf,
            page,
            zoom,
            rotate,
            screen,
            x,
            y,
            night,
            dpi,
            quality,
            out,
        }),
        CliCmd::Simulate {
            pdf,
            keys,
            screen,
            out_dir,
        } => cmd_simulate(pdf, keys, screen, out_dir),
        CliCmd::Outline { pdf } => cmd_outline(pdf),
        CliCmd::Command { pdf, line } => cmd_command(pdf, line),
    }
}

fn cmd_info(pdf: PathBuf) -> Result<()> {
    let doc = PdfDocument::open(&pdf)?;
    println!("{}", pdf.display());
    println!("  pages: {}", doc.page_count());
    for i in 0..doc.page_count().min(5) {
        let s = doc.page_size(i)?;
        println!("  page {}: {:.1} x {:.1}", i + 1, s.width, s.height);
    }
    Ok(())
}

struct RenderArgs {
    pdf: PathBuf,
    page: usize,
    zoom: String,
    rotate: i32,
    screen: String,
    x: i32,
    y: i32,
    night: bool,
    dpi: Option<f32>,
    quality: f32,
    out: PathBuf,
}

fn cmd_render(a: RenderArgs) -> Result<()> {
    let doc = PdfDocument::open(&a.pdf)?;
    let (w, h) = parse_screen(&a.screen)?;
    let mut viewport = Viewport {
        page_idx: a.page.saturating_sub(1),
        x_off: a.x,
        y_off: a.y,
        zoom: parse_zoom(&a.zoom)?,
        rotation: Rotation::from_degrees(a.rotate),
        screen_w: w,
        screen_h: h,
        night_mode: a.night,
        render_dpi: a.dpi,
        render_quality: (a.quality / 100.0).clamp(0.1, 2.0),
    };
    // If x_off/y_off weren't explicitly set (both 0) we let Navigator
    // snap to the zoom anchor so narrow pages sit centered.
    if a.x == 0 && a.y == 0 {
        let z = viewport.zoom;
        Navigator::apply(&doc, &mut viewport, Action::SetZoom(z))?;
    }
    let frame = Renderer::render(&doc, &viewport)?;
    frame
        .composed
        .save(&a.out)
        .with_context(|| format!("writing {:?}", a.out))?;
    eprintln!(
        "rendered page {} @ {}x{} → {}  (render={:?} compose={:?})",
        viewport.page_idx + 1,
        viewport.screen_w,
        viewport.screen_h,
        a.out.display(),
        frame.render,
        frame.compose,
    );
    Ok(())
}

fn cmd_simulate(pdf: PathBuf, keys: String, screen: String, out_dir: PathBuf) -> Result<()> {
    let doc = PdfDocument::open(&pdf)?;
    let (w, h) = parse_screen(&screen)?;
    std::fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {:?}", out_dir))?;
    let mut viewport = Viewport {
        screen_w: w,
        screen_h: h,
        ..Viewport::default()
    };
    // Snap to anchor for first frame.
    let z = viewport.zoom;
    Navigator::apply(&doc, &mut viewport, Action::SetZoom(z))?;
    let mut state = KeyParserState::default();
    let mut frame_idx = 0usize;
    // Dump initial frame.
    dump_frame(&doc, &viewport, &out_dir, frame_idx)?;
    frame_idx += 1;

    for key in parse_key_sequence(&keys)? {
        match KeyParser::feed(&mut state, key) {
            KeyOutcome::Action { action, count } => {
                for _ in 0..count {
                    Navigator::apply(&doc, &mut viewport, action.clone())?;
                }
                dump_frame(&doc, &viewport, &out_dir, frame_idx)?;
                frame_idx += 1;
            }
            KeyOutcome::Pending
            | KeyOutcome::OpenCommand
            | KeyOutcome::ToggleHelp
            | KeyOutcome::Window(_) => {}
            KeyOutcome::Quit => break,
        }
    }
    eprintln!("wrote {frame_idx} frames to {:?}", out_dir);
    Ok(())
}

fn cmd_outline(pdf: PathBuf) -> Result<()> {
    let doc = PdfDocument::open(&pdf)?;
    let outline = doc.outline()?;
    fn walk(o: &[svreader_core::Outline], depth: usize) {
        for item in o {
            let pad = "  ".repeat(depth);
            let page = item.page.map(|p| format!(" p{}", p + 1)).unwrap_or_default();
            println!("{pad}{}{}", item.title, page);
            walk(&item.children, depth + 1);
        }
    }
    walk(&outline, 0);
    Ok(())
}

fn cmd_command(pdf: PathBuf, line: String) -> Result<()> {
    let doc = PdfDocument::open(&pdf)?;
    let registry = CommandRegistry::default();
    let parsed = registry.parse(&line)?;
    println!("parsed: {:?}", parsed);
    let mut viewport = Viewport::default();
    if let ParsedCommand::Nav(action) = parsed {
        Navigator::apply(&doc, &mut viewport, action)?;
        println!("viewport after: page={} zoom={:?} rot={}", viewport.page_idx + 1, viewport.zoom, viewport.rotation.degrees());
    }
    Ok(())
}

fn dump_frame<D: Document>(doc: &D, viewport: &Viewport, dir: &PathBuf, idx: usize) -> Result<()> {
    let frame = Renderer::render(doc, viewport)?;
    let path = dir.join(format!("frame_{idx:04}.png"));
    frame.composed.save(&path)?;
    Ok(())
}

fn parse_screen(s: &str) -> Result<(u32, u32)> {
    let (a, b) = s
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("--screen wants WxH (got {s:?})"))?;
    Ok((a.parse()?, b.parse()?))
}

fn parse_zoom(s: &str) -> Result<ZoomMode> {
    match s {
        "fit-w" | "fitwidth" | "width" => Ok(ZoomMode::FitWidth),
        "fit-h" | "fitheight" | "height" => Ok(ZoomMode::FitHeight),
        "fit-p" | "fitpage" | "page" => Ok(ZoomMode::FitPage),
        s if s.ends_with('%') => {
            let p: f32 = s.trim_end_matches('%').parse()?;
            Ok(ZoomMode::Custom(p / 100.0))
        }
        _ => bail!("bad --zoom {s:?}"),
    }
}

/// Parse a short vim-like key sequence: `gg`, `3j`, `<C-f>`, `<Esc>`,
/// `+`, `-`, regular chars. Not a full parser — just enough for tests.
fn parse_key_sequence(s: &str) -> Result<Vec<Key>> {
    let mut keys = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            let end = s[i..]
                .find('>')
                .ok_or_else(|| anyhow::anyhow!("unclosed < in key sequence"))?
                + i;
            let token = &s[i + 1..end];
            keys.push(parse_special(token)?);
            i = end + 1;
        } else {
            keys.push(Key::Char(b as char));
            i += 1;
        }
    }
    Ok(keys)
}

fn parse_special(token: &str) -> Result<Key> {
    match token {
        "Esc" => Ok(Key::Esc),
        "Enter" | "CR" => Ok(Key::Enter),
        "Tab" => Ok(Key::Tab),
        "S-Tab" => Ok(Key::BackTab),
        "Up" => Ok(Key::Up),
        "Down" => Ok(Key::Down),
        "Left" => Ok(Key::Left),
        "Right" => Ok(Key::Right),
        "PageUp" => Ok(Key::PageUp),
        "PageDown" => Ok(Key::PageDown),
        "Home" => Ok(Key::Home),
        "End" => Ok(Key::End),
        s if s.starts_with("C-") && s.len() == 3 => {
            Ok(Key::Ctrl(s.as_bytes()[2] as char))
        }
        _ => bail!("unknown key token <{token}>"),
    }
}

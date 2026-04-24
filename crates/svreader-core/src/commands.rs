use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::navigator::Action;
use crate::viewport::ZoomMode;

/// Argument spec for completion hinting. Kept deliberately minimal —
/// we don't need a full parser, just enough for the palette to show
/// "this wants a number" / "this wants one-of".
#[derive(Debug, Clone)]
pub enum CommandArg {
    None,
    Number,
    OneOf(Vec<&'static str>),
    /// Anything — typically a path.
    Free,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub arg: CommandArg,
}

/// Orientation for a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Parsed, executable effect of a command. The TUI executes these —
/// some need a Navigator action, most are window- or workspace-level.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedCommand {
    Nav(Action),

    /// Hard-quit the entire application. `:qa`, `:qall`.
    Quit,
    /// Close the focused window; quit if it's the last one. `:close`, `:q`.
    CloseWindow,
    /// Close all other windows in the current tab. `:only`.
    OnlyWindow,

    /// Split focused window. `file` absent → share current buffer
    /// (two views of the same PDF).
    Split {
        direction: SplitDirection,
        file: Option<PathBuf>,
    },

    /// Open/load a file into the current window. `:edit`, `:open`.
    Edit(PathBuf),

    /// `:Explore [path]` / `:Sexplore` / `:Vexplore`. The optional
    /// `split` means "make a new split first, then put the explorer
    /// there"; `None` replaces the current window's buffer.
    Explore {
        split: Option<SplitDirection>,
        path: Option<PathBuf>,
    },

    /// `:tabnew [file]` — new tab, optionally preloaded with a file.
    TabNew(Option<PathBuf>),
    /// `:tabclose` — close current tab.
    TabClose,
    /// `:tabonly` — close all other tabs.
    TabOnly,

    /// `:b <n>` — jump to buffer N in the current window.
    Buffer(usize),
    /// `:bn` — next buffer in list.
    BufferNext,
    /// `:bp` — previous buffer in list.
    BufferPrev,

    /// `:tabmove ±N` — reorder the current tab relatively. `:tabmove N`
    /// (no sign) is treated as relative too for simplicity.
    TabMove(i32),
    /// `:resize ±N` — adjust the current window's height by N rows.
    Resize(i32),
    /// `:vresize ±N` — adjust the current window's width by N cols.
    /// Also produced by `:vertical resize ±N`.
    VResize(i32),

    Help,
    CacheSet(bool),
    CacheToggle,
    CacheSize(usize),
    /// `:ecache on|off|toggle` — controls the encoded-frame cache.
    ECacheSet(bool),
    ECacheToggle,
    /// `:ecache-size N` — sets the encoded-frame cache capacity.
    ECacheSize(usize),
    Prefetch(usize),
    Reset,
    /// Pick the sixel palette. Grayscale is fastest for text-heavy
    /// PDFs; xterm256 is the default, good for mixed content.
    Colors(ColorPalette),
    /// `:copy` — copy the focused window's current page to the system
    /// clipboard as a PNG image.
    CopyPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPalette {
    XTerm256,
    Grayscale,
}

pub struct CommandRegistry {
    commands: Vec<Command>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self {
            commands: all_commands(),
        }
    }
}

impl CommandRegistry {
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    pub fn lookup(&self, name: &str) -> Option<&Command> {
        self.commands
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))
    }

    /// Prefix-match commands for palette completion. Returns exact
    /// primary names (not aliases) in registration order.
    pub fn complete(&self, prefix: &str) -> Vec<&Command> {
        self.commands
            .iter()
            .filter(|c| c.name.starts_with(prefix) || c.aliases.iter().any(|a| a.starts_with(prefix)))
            .collect()
    }

    /// Parse a full command line like `zoom fit-w` or `goto 42`.
    ///
    /// Vim's `:vertical resize N` prefix syntax is supported by
    /// rewriting to `:vresize N` before dispatch.
    pub fn parse(&self, line: &str) -> Result<ParsedCommand> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("empty command"));
        }
        // Rewrite vim's `:vertical resize N` as `:vresize N`.
        let rewritten = if let Some(rest) = trimmed.strip_prefix("vertical ") {
            let rest = rest.trim_start();
            if let Some(args) = rest.strip_prefix("resize") {
                format!("vresize{}", args)
            } else {
                return Err(anyhow!("only `:vertical resize` is supported"));
            }
        } else {
            trimmed.to_string()
        };
        let mut parts = rewritten.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap();
        let arg = parts.next().unwrap_or("").trim();
        let cmd = self
            .lookup(name)
            .ok_or_else(|| anyhow!("unknown command: {name}"))?;
        parse_command(cmd, arg)
    }
}

fn parse_command(cmd: &Command, arg: &str) -> Result<ParsedCommand> {
    match cmd.name {
        "qall" => Ok(ParsedCommand::Quit),
        "quit" => Ok(ParsedCommand::CloseWindow),
        "close" => Ok(ParsedCommand::CloseWindow),
        "only" => Ok(ParsedCommand::OnlyWindow),

        "split" => Ok(ParsedCommand::Split {
            direction: SplitDirection::Horizontal,
            file: opt_path(arg),
        }),
        "vsplit" => Ok(ParsedCommand::Split {
            direction: SplitDirection::Vertical,
            file: opt_path(arg),
        }),
        "edit" => {
            let p = require_path(arg, ":edit wants a path")?;
            Ok(ParsedCommand::Edit(p))
        }
        "open" => {
            let p = require_path(arg, ":open wants a path")?;
            Ok(ParsedCommand::Edit(p))
        }

        "Explore" => Ok(ParsedCommand::Explore {
            split: None,
            path: opt_path(arg),
        }),
        "Sexplore" => Ok(ParsedCommand::Explore {
            split: Some(SplitDirection::Horizontal),
            path: opt_path(arg),
        }),
        "Vexplore" => Ok(ParsedCommand::Explore {
            split: Some(SplitDirection::Vertical),
            path: opt_path(arg),
        }),

        "tabnew" => Ok(ParsedCommand::TabNew(opt_path(arg))),
        "tabclose" => Ok(ParsedCommand::TabClose),
        "tabonly" => Ok(ParsedCommand::TabOnly),

        "buffer" => {
            let n: usize = arg
                .parse()
                .map_err(|_| anyhow!(":b wants a buffer index"))?;
            Ok(ParsedCommand::Buffer(n))
        }
        "bnext" => Ok(ParsedCommand::BufferNext),
        "bprev" => Ok(ParsedCommand::BufferPrev),

        "tabmove" => {
            let n = parse_signed(arg).map_err(|_| anyhow!(":tabmove wants +N, -N, or N"))?;
            Ok(ParsedCommand::TabMove(n))
        }
        "resize" => {
            let n = parse_signed(arg).map_err(|_| anyhow!(":resize wants +N, -N, or N"))?;
            Ok(ParsedCommand::Resize(n))
        }
        "vresize" => {
            let n = parse_signed(arg).map_err(|_| anyhow!(":vresize wants +N, -N, or N"))?;
            Ok(ParsedCommand::VResize(n))
        }

        "help" => Ok(ParsedCommand::Help),
        "goto" => {
            let n: usize = arg.parse().map_err(|_| anyhow!(":goto N requires a number"))?;
            Ok(ParsedCommand::Nav(Action::GotoPage(n.saturating_sub(1))))
        }
        "first" => Ok(ParsedCommand::Nav(Action::FirstPage)),
        "last" => Ok(ParsedCommand::Nav(Action::LastPage)),
        "next" => Ok(ParsedCommand::Nav(Action::NextPage)),
        "prev" => Ok(ParsedCommand::Nav(Action::PrevPage)),
        "top" => Ok(ParsedCommand::Nav(Action::PageTop)),
        "bottom" => Ok(ParsedCommand::Nav(Action::PageBottom)),
        "middle" => Ok(ParsedCommand::Nav(Action::PageMiddle)),
        "reset" => Ok(ParsedCommand::Reset),
        "zoom" => {
            let z = match arg {
                "fit-w" | "fitwidth" | "width" | "w" => ZoomMode::FitWidth,
                "fit-h" | "fitheight" | "height" | "h" => ZoomMode::FitHeight,
                "fit-p" | "fitpage" | "page" | "p" => ZoomMode::FitPage,
                other => {
                    let mult: f32 = other
                        .trim_end_matches('%')
                        .parse()
                        .map_err(|_| anyhow!(":zoom wants fit-w|fit-h|fit-p|NN% (got {other:?})"))?;
                    let mult = if other.ends_with('%') { mult / 100.0 } else { mult };
                    ZoomMode::Custom(mult)
                }
            };
            Ok(ParsedCommand::Nav(Action::SetZoom(z)))
        }
        "rotate" => {
            let action = match arg {
                "cw" => Action::RotateCw,
                "ccw" => Action::RotateCcw,
                other => {
                    let deg: i32 = other
                        .parse()
                        .map_err(|_| anyhow!(":rotate wants 0|90|180|270 or cw|ccw"))?;
                    Action::SetRotation(crate::viewport::Rotation::from_degrees(deg))
                }
            };
            Ok(ParsedCommand::Nav(action))
        }
        "night" => {
            let action = match arg {
                "on" => Action::SetNight(true),
                "off" => Action::SetNight(false),
                "toggle" | "" => Action::ToggleNight,
                other => return Err(anyhow!(":night wants on|off|toggle (got {other:?})")),
            };
            Ok(ParsedCommand::Nav(action))
        }
        "dpi" => {
            let action = if arg == "auto" || arg.is_empty() {
                Action::SetRenderDpi(None)
            } else {
                let n: f32 = arg
                    .parse()
                    .map_err(|_| anyhow!(":dpi wants a number or 'auto'"))?;
                Action::SetRenderDpi(Some(n))
            };
            Ok(ParsedCommand::Nav(action))
        }
        "quality" => {
            let pct: f32 = arg
                .trim_end_matches('%')
                .parse()
                .map_err(|_| anyhow!(":quality wants a percentage 10..200"))?;
            let q = (pct / 100.0).clamp(0.1, 2.0);
            Ok(ParsedCommand::Nav(Action::SetRenderQuality(q)))
        }
        "cache" => match arg {
            "on" => Ok(ParsedCommand::CacheSet(true)),
            "off" => Ok(ParsedCommand::CacheSet(false)),
            "toggle" | "" => Ok(ParsedCommand::CacheToggle),
            other => Err(anyhow!(":cache wants on|off|toggle (got {other:?})")),
        },
        "cache-size" => {
            let n: usize = arg
                .parse()
                .map_err(|_| anyhow!(":cache-size wants a positive integer"))?;
            Ok(ParsedCommand::CacheSize(n.max(1)))
        }
        "ecache" => match arg {
            "on" => Ok(ParsedCommand::ECacheSet(true)),
            "off" => Ok(ParsedCommand::ECacheSet(false)),
            "toggle" | "" => Ok(ParsedCommand::ECacheToggle),
            other => Err(anyhow!(":ecache wants on|off|toggle (got {other:?})")),
        },
        "ecache-size" => {
            let n: usize = arg
                .parse()
                .map_err(|_| anyhow!(":ecache-size wants a positive integer"))?;
            Ok(ParsedCommand::ECacheSize(n.max(1)))
        }
        "prefetch" => {
            let n: usize = arg
                .parse()
                .map_err(|_| anyhow!(":prefetch wants a non-negative integer"))?;
            Ok(ParsedCommand::Prefetch(n))
        }
        "colors" => match arg {
            "xterm256" | "256" | "color" => Ok(ParsedCommand::Colors(ColorPalette::XTerm256)),
            "gray" | "grey" | "grayscale" | "g8" => Ok(ParsedCommand::Colors(ColorPalette::Grayscale)),
            other => Err(anyhow!(":colors wants xterm256|gray (got {other:?})")),
        },
        "copy" => Ok(ParsedCommand::CopyPage),
        other => Err(anyhow!("command {other:?} has no handler")),
    }
}

/// Parse an integer that may carry a leading `+` or `-` sign. `+N`
/// and `N` are both treated as positive.
fn parse_signed(s: &str) -> Result<i32> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty"));
    }
    if let Some(rest) = s.strip_prefix('+') {
        let n: i32 = rest.parse()?;
        Ok(n)
    } else {
        let n: i32 = s.parse()?;
        Ok(n)
    }
}

fn opt_path(arg: &str) -> Option<PathBuf> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(expand_home(trimmed)))
    }
}

fn require_path(arg: &str, ctx: &'static str) -> Result<PathBuf> {
    match opt_path(arg) {
        Some(p) => Ok(p),
        None => Err(anyhow!(ctx)),
    }
}

/// Expand a leading `~/` to `$HOME` so `:e ~/foo.pdf` does what vim
/// does.
fn expand_home(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            p.push(rest);
            return p.to_string_lossy().into_owned();
        }
    }
    s.to_string()
}

fn all_commands() -> Vec<Command> {
    vec![
        // Window/tab manipulation first — these are new and discoverable.
        Command {
            name: "split",
            aliases: &["sp"],
            description: "Horizontal split (optionally open a file)",
            arg: CommandArg::Free,
        },
        Command {
            name: "vsplit",
            aliases: &["vsp"],
            description: "Vertical split (optionally open a file)",
            arg: CommandArg::Free,
        },
        Command {
            name: "close",
            aliases: &["clo"],
            description: "Close current window",
            arg: CommandArg::None,
        },
        Command {
            name: "only",
            aliases: &["on"],
            description: "Close all other windows in current tab",
            arg: CommandArg::None,
        },
        Command {
            name: "Explore",
            aliases: &["Ex"],
            description: "File explorer in current window",
            arg: CommandArg::Free,
        },
        Command {
            name: "Sexplore",
            aliases: &["Sex"],
            description: "Horizontal split + file explorer",
            arg: CommandArg::Free,
        },
        Command {
            name: "Vexplore",
            aliases: &["Vex"],
            description: "Vertical split + file explorer",
            arg: CommandArg::Free,
        },
        Command {
            name: "tabnew",
            aliases: &["tabe"],
            description: "Open a new tab (optionally with a file)",
            arg: CommandArg::Free,
        },
        Command {
            name: "tabclose",
            aliases: &["tabc"],
            description: "Close the current tab",
            arg: CommandArg::None,
        },
        Command {
            name: "tabonly",
            aliases: &["tabo"],
            description: "Close all other tabs",
            arg: CommandArg::None,
        },
        Command {
            name: "edit",
            aliases: &["e"],
            description: "Load file into current window",
            arg: CommandArg::Free,
        },
        Command {
            name: "open",
            aliases: &["o"],
            description: "Open a file in the current window (same as :edit)",
            arg: CommandArg::Free,
        },
        Command {
            name: "buffer",
            aliases: &["b"],
            description: "Show buffer N in current window",
            arg: CommandArg::Number,
        },
        Command {
            name: "bnext",
            aliases: &["bn"],
            description: "Next buffer in list",
            arg: CommandArg::None,
        },
        Command {
            name: "bprev",
            aliases: &["bp"],
            description: "Previous buffer in list",
            arg: CommandArg::None,
        },
        Command {
            name: "quit",
            // `:q` is close-window (vim semantics), not quit-app.
            aliases: &["q"],
            description: "Close current window (quit if last)",
            arg: CommandArg::None,
        },
        Command {
            name: "tabmove",
            aliases: &["tabm"],
            description: "Move current tab by ±N positions",
            arg: CommandArg::Number,
        },
        Command {
            name: "resize",
            aliases: &["res"],
            description: "Adjust current window height by ±N rows",
            arg: CommandArg::Number,
        },
        Command {
            name: "vresize",
            aliases: &["vres"],
            description: "Adjust current window width by ±N cols",
            arg: CommandArg::Number,
        },
        Command {
            name: "qall",
            aliases: &["qa", "exit"],
            description: "Quit svreader",
            arg: CommandArg::None,
        },

        // Reading commands (existing).
        Command {
            name: "goto",
            aliases: &["jump"],
            description: "Go to page N (1-indexed)",
            arg: CommandArg::Number,
        },
        Command {
            name: "zoom",
            aliases: &[],
            description: "Set zoom mode (fit-w, fit-h, fit-p, 125%, ...)",
            arg: CommandArg::OneOf(vec!["fit-w", "fit-h", "fit-p"]),
        },
        Command {
            name: "rotate",
            aliases: &[],
            description: "Rotate page (90, 180, 270, 0)",
            arg: CommandArg::OneOf(vec!["0", "90", "180", "270"]),
        },
        Command {
            name: "first",
            aliases: &[],
            description: "Jump to first page",
            arg: CommandArg::None,
        },
        Command {
            name: "last",
            aliases: &[],
            description: "Jump to last page",
            arg: CommandArg::None,
        },
        Command {
            name: "next",
            aliases: &[],
            description: "Next page (no overlap)",
            arg: CommandArg::None,
        },
        Command {
            name: "prev",
            aliases: &[],
            description: "Previous page (no overlap)",
            arg: CommandArg::None,
        },
        Command {
            name: "top",
            aliases: &[],
            description: "Scroll to top of current page",
            arg: CommandArg::None,
        },
        Command {
            name: "bottom",
            aliases: &[],
            description: "Scroll to bottom of current page",
            arg: CommandArg::None,
        },
        Command {
            name: "middle",
            aliases: &[],
            description: "Scroll to middle of current page",
            arg: CommandArg::None,
        },
        Command {
            name: "reset",
            aliases: &[],
            description: "Reset zoom/rotation/scroll to defaults",
            arg: CommandArg::None,
        },
        Command {
            name: "help",
            aliases: &[],
            description: "Toggle keybindings overlay",
            arg: CommandArg::None,
        },
        Command {
            name: "night",
            aliases: &[],
            description: "Night mode (on|off|toggle)",
            arg: CommandArg::OneOf(vec!["on", "off", "toggle"]),
        },
        Command {
            name: "dpi",
            aliases: &[],
            description: "Raster DPI override (N or 'auto')",
            arg: CommandArg::Free,
        },
        Command {
            name: "quality",
            aliases: &[],
            description: "Sixel quality % (10..200)",
            arg: CommandArg::Number,
        },
        Command {
            name: "cache",
            aliases: &[],
            description: "Enable/disable page cache",
            arg: CommandArg::OneOf(vec!["on", "off", "toggle"]),
        },
        Command {
            name: "cache-size",
            aliases: &[],
            description: "Set RenderCache LRU capacity (pages)",
            arg: CommandArg::Number,
        },
        Command {
            name: "ecache",
            aliases: &[],
            description: "Enable/disable encoded-frame cache",
            arg: CommandArg::OneOf(vec!["on", "off", "toggle"]),
        },
        Command {
            name: "ecache-size",
            aliases: &[],
            description: "Set encoded-frame cache capacity (frames)",
            arg: CommandArg::Number,
        },
        Command {
            name: "prefetch",
            aliases: &[],
            description: "Pages to prefetch each direction",
            arg: CommandArg::Number,
        },
        Command {
            name: "colors",
            aliases: &[],
            description: "Sixel palette (xterm256 or gray)",
            arg: CommandArg::OneOf(vec!["xterm256", "gray"]),
        },
        Command {
            name: "copy",
            aliases: &[],
            description: "Copy current page to clipboard as image",
            arg: CommandArg::None,
        },
    ]
}

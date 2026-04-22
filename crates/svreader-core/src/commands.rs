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
    Free,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub arg: CommandArg,
}

/// Parsed, executable effect of a command. The TUI executes these —
/// some need a Navigator action, some are UI-only (quit, help, cache
/// control), which we'll represent explicitly.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedCommand {
    Nav(Action),
    Quit,
    Help,
    CacheSet(bool),
    CacheToggle,
    CacheSize(usize),
    Prefetch(usize),
    Reset,
    /// Pick the sixel palette. Grayscale is fastest for text-heavy
    /// PDFs; xterm256 is the default, good for mixed content.
    Colors(ColorPalette),
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
    pub fn parse(&self, line: &str) -> Result<ParsedCommand> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("empty command"));
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
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
        "quit" => Ok(ParsedCommand::Quit),
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
        other => Err(anyhow!("command {other:?} has no handler")),
    }
}

fn all_commands() -> Vec<Command> {
    vec![
        Command {
            name: "goto",
            aliases: &["g", "jump"],
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
            aliases: &["h"],
            description: "Toggle keybindings overlay",
            arg: CommandArg::None,
        },
        Command {
            name: "quit",
            aliases: &["q", "exit"],
            description: "Exit svreader",
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
            description: "Set LRU capacity (pages)",
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
    ]
}

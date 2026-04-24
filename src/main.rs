use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

/// svreader — terminal PDF reader with vim keybindings.
#[derive(Parser, Debug)]
#[command(name = "svreader", version, about)]
struct Cli {
    /// PDF file to open, or a directory to browse in the explorer.
    /// Omit to land in an explorer rooted at the current working
    /// directory.
    pdf: Option<PathBuf>,

    /// Override terminal pixel size (format: WxH) for debugging.
    #[arg(long, env = "SVREADER_SCREEN_PX")]
    screen_px: Option<String>,

    /// Start at page number (1-indexed). Overrides sidecar.
    #[arg(long)]
    page: Option<usize>,
}

fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    svreader_tui::run(svreader_tui::RunOptions {
        pdf: cli.pdf,
        screen_px_override: cli.screen_px,
        start_page: cli.page,
    })
}

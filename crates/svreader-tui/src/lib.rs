//! svreader-tui — terminal front-end. Sixel output, vim input.
//!
//! M1 keeps this deliberately thin: crossterm for input + screen
//! control, icy_sixel for the image, and hand-written ANSI for the
//! status bar / command palette / help overlay. Ratatui is slated for
//! heavier overlays post-M1; mixing it with sixel is fiddly and this
//! crate's job is to be boring.

mod capabilities;
mod render_loop;
mod sixel_write;
mod terminal;
mod timings;
mod tmux;
mod ui;

pub mod bench {
    //! Tiny re-exports so examples can benchmark the encoder without
    //! us exposing the internals to everyone else.
    pub use crate::sixel_write::{encode_and_write as encode_and_write_bench, ColorMode, SixelEmitTiming};
}

use std::path::PathBuf;

pub struct RunOptions {
    pub pdf: PathBuf,
    pub screen_px_override: Option<String>,
    pub start_page: Option<usize>,
}

pub fn run(opts: RunOptions) -> anyhow::Result<()> {
    render_loop::run(opts)
}

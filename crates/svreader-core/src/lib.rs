//! svreader-core — pure backend for svreader.
//!
//! No terminal dependencies. Every rendering decision here must be
//! reproducible from the CLI as a PNG on disk.

pub mod cache;
pub mod commands;
pub mod docstate;
pub mod document;
pub mod keys;
pub mod navigator;
pub mod pdf;
pub mod prefetch;
pub mod renderer;
pub mod viewport;

pub use cache::{CacheKey, CachedPage, PageCache};
pub use commands::{Command, CommandArg, CommandRegistry, ColorPalette, ParsedCommand};
pub use docstate::DocState;
pub use document::{Document, Outline, PageSize};
pub use keys::{Key, KeyParser, KeyParserState};
pub use navigator::{Action, Navigator};
pub use pdf::PdfDocument;
pub use prefetch::{PrefetchRequest, Prefetcher};
pub use renderer::{ComposeTiming, RenderTiming, RenderedFrame, Renderer};
pub use viewport::{Rotation, Viewport, ZoomMode};

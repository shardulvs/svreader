//! svreader-core — pure backend for svreader.
//!
//! No terminal dependencies. Every rendering decision here must be
//! reproducible from the CLI as a PNG on disk.

pub mod buffer;
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

pub use buffer::{
    Buffer, BufferId, BufferIdSource, ExplorerBuffer, ExplorerEntry, ExplorerKind, PdfBuffer,
    EXPLORER_SUPPORTED_EXTS,
};
pub use cache::{CacheKey, CachedPage, RenderCache};
pub use commands::{
    ColorPalette, Command, CommandArg, CommandRegistry, ParsedCommand, SplitDirection,
};
pub use docstate::DocState;
pub use document::{Document, Outline, PageInfo, PageMetrics, PageSize};
pub use keys::{
    ArrowDir, Key, KeyOutcome, KeyParser, KeyParserState, Leader, PageDir, WindowOp,
};
pub use navigator::{Action, Navigator};
pub use pdf::PdfDocument;
pub use prefetch::{PrefetchRequest, Prefetcher};
pub use renderer::{ComposeTiming, RenderTiming, RenderedFrame, Renderer};
pub use viewport::{Rotation, Viewport, ZoomMode};

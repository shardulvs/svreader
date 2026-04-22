//! Open-buffer bookkeeping.
//!
//! M1.5a only cares about PDF buffers, but `BufferId` is already
//! in the cache key so multiple concurrent PDFs share one
//! `PageCache` without collisions. When M1.5b adds the netrw-like
//! explorer, we promote this module to an `enum Buffer { Pdf, Explorer }`
//! and add a trait there.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::Result;

use crate::cache::PageCache;
use crate::docstate::DocState;
use crate::pdf::PdfDocument;
use crate::prefetch::Prefetcher;

/// Stable identifier for an open buffer. Drives `CacheKey` so two
/// PDFs open at once don't mix up their raster bitmaps.
///
/// Values are handed out by `BufferIdSource::next()`, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u32);

#[derive(Debug, Default)]
pub struct BufferIdSource {
    next: AtomicU32,
}

impl BufferIdSource {
    pub fn new() -> Self {
        // Start at 1 so 0 is a harmless "unset" sentinel.
        Self {
            next: AtomicU32::new(1),
        }
    }

    pub fn next(&self) -> BufferId {
        BufferId(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

/// A PDF the user has opened. One instance per distinct path; two
/// windows can hold `Arc`s to the same buffer for vim-style shared
/// buffers.
pub struct PdfBuffer {
    pub id: BufferId,
    pub path: PathBuf,
    pub pdf: PdfDocument,
    pub state: DocState,
    /// Per-buffer prefetcher: owns its own mupdf handle (not `Send`)
    /// and dies with the buffer.
    pub prefetcher: Prefetcher,
}

impl PdfBuffer {
    pub fn open(
        id: BufferId,
        path: impl AsRef<Path>,
        cache: Arc<PageCache>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let pdf = PdfDocument::open(&path)?;
        let state = DocState::load(&path).unwrap_or_default();
        let prefetcher = Prefetcher::spawn(&pdf, cache)?;
        Ok(Self {
            id,
            path,
            pdf,
            state,
            prefetcher,
        })
    }

    /// Filename for display (falls back to "document").
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into())
    }
}

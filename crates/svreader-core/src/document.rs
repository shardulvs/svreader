use std::sync::Arc;

use anyhow::{anyhow, Result};
use image::RgbaImage;

use crate::viewport::Rotation;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// The navigation-relevant subset of a document: how many pages and
/// how big each one is. That's all `Navigator` actually reads —
/// factoring this out lets us run the same navigation logic on a
/// background thread (where mupdf handles can't be carried) so the
/// ECache filler predicts target viewports via the real Navigator
/// instead of a second, drift-prone reimplementation.
///
/// Supertrait of `Document`: every document is trivially a
/// `PageMetrics`, but the reverse is not true — a `PageInfo`
/// snapshot is a `PageMetrics` without being able to render pages.
pub trait PageMetrics {
    fn page_count(&self) -> usize;
    fn page_size(&self, page_idx: usize) -> Result<PageSize>;
}

/// `Send`-able snapshot of a document's page geometry. Built once
/// when the PDF is opened and shared (via `Arc`) with any background
/// worker that needs to run Navigator without holding the mupdf
/// handle itself.
#[derive(Debug, Clone)]
pub struct PageInfo {
    sizes: Arc<Vec<PageSize>>,
}

impl PageInfo {
    /// Eagerly pull page dimensions for every page. Page size lookups
    /// are cheap in mupdf (parse the page's MediaBox, no rendering),
    /// and computing up front means no lock contention with the
    /// render path later.
    pub fn from_metrics<M: PageMetrics + ?Sized>(m: &M) -> Result<Self> {
        let n = m.page_count();
        let mut sizes = Vec::with_capacity(n);
        for i in 0..n {
            sizes.push(m.page_size(i)?);
        }
        Ok(Self {
            sizes: Arc::new(sizes),
        })
    }
}

impl PageMetrics for PageInfo {
    fn page_count(&self) -> usize {
        self.sizes.len()
    }
    fn page_size(&self, page_idx: usize) -> Result<PageSize> {
        self.sizes
            .get(page_idx)
            .copied()
            .ok_or_else(|| anyhow!("page {page_idx} out of range ({})", self.sizes.len()))
    }
}

#[derive(Debug, Clone)]
pub struct Outline {
    pub title: String,
    pub page: Option<usize>,
    pub children: Vec<Outline>,
}

/// A document that svreader can display. Kept tight and synchronous —
/// implementations may not be `Send`/`Sync` (mupdf isn't), so callers
/// hold one per thread.
///
/// `page_count` / `page_size` live in the `PageMetrics` supertrait;
/// implementors provide them there.
pub trait Document: PageMetrics {
    /// Rasterise a page at the given pixel scale and rotation.
    ///
    /// `scale` is "pixels per point" — 1.0 means 72 DPI. The returned
    /// RGBA image is opaque (alpha always 255) so downstream compose
    /// steps can treat it as solid.
    fn render_page(
        &self,
        page_idx: usize,
        scale: f32,
        rotation: Rotation,
    ) -> Result<RgbaImage>;

    fn outline(&self) -> Result<Vec<Outline>> {
        Ok(Vec::new())
    }

    fn page_text(&self, _page_idx: usize) -> Result<String> {
        Ok(String::new())
    }
}

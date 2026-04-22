use anyhow::Result;
use image::RgbaImage;

use crate::viewport::Rotation;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
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
pub trait Document {
    fn page_count(&self) -> usize;

    /// Native page size in PDF points.
    fn page_size(&self, page_idx: usize) -> Result<PageSize>;

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

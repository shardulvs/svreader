use std::time::Duration;

use anyhow::Result;
use image::RgbaImage;

use crate::cache::CachedPage;
use crate::document::Document;
use crate::viewport::Viewport;

/// Per-page pixel padding color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadColor(pub [u8; 4]);

impl PadColor {
    pub fn for_viewport(v: &Viewport) -> Self {
        if v.night_mode {
            PadColor([0x28, 0x28, 0x28, 0xFF]) // dark grey
        } else {
            PadColor([0x00, 0x00, 0x00, 0xFF]) // black
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RenderTiming {
    pub render: Duration,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ComposeTiming {
    pub compose: Duration,
}

pub struct RenderedFrame {
    pub page: CachedPage,
    pub composed: RgbaImage,
    pub render: Duration,
    pub compose: Duration,
}

pub struct Renderer;

impl Renderer {
    /// Rasterise the page at the raster scale dictated by the viewport.
    /// Expensive — dominated by mupdf's work. Does NOT apply night
    /// inversion or crop; those are handled in compose() so the cache
    /// can reuse the bitmap.
    pub fn render_page<D: Document>(
        doc: &D,
        viewport: &Viewport,
    ) -> Result<(CachedPage, RenderTiming)> {
        let page_size = doc.page_size(viewport.page_idx)?;
        let raster_scale = viewport.raster_scale(page_size);
        let display_scale = viewport.display_scale(page_size);

        let start = std::time::Instant::now();
        let raw = doc.render_page(viewport.page_idx, raster_scale, viewport.rotation)?;

        // If raster_scale != display_scale (because of dpi override or
        // quality ratio), resize down/up to match the screen layout.
        let image = if (raster_scale - display_scale).abs() < 1e-3 {
            raw
        } else {
            resize_rgba(raw, display_scale / raster_scale)
        };
        let render = start.elapsed();

        let cached = CachedPage {
            page_idx: viewport.page_idx,
            rotation: viewport.rotation,
            display_scale,
            image,
        };
        Ok((cached, RenderTiming { render }))
    }

    /// Crop the rasterised page into a screen-sized RGBA image,
    /// padding with `PadColor` where the page doesn't cover, and
    /// inverting per-pixel if night mode is on.
    pub fn compose(
        page: &CachedPage,
        viewport: &Viewport,
    ) -> (RgbaImage, ComposeTiming) {
        let start = std::time::Instant::now();
        let pad = PadColor::for_viewport(viewport).0;
        let sw = viewport.screen_w.max(1);
        let sh = viewport.screen_h.max(1);
        let pw = page.image.width();
        let ph = page.image.height();

        let mut out = RgbaImage::new(sw, sh);
        {
            let buf = out.as_mut();
            // Fill with pad color up front.
            for px in buf.chunks_exact_mut(4) {
                px.copy_from_slice(&pad);
            }
        }

        // Where does the page sit inside the screen?
        //   screen_x = page_x - x_off
        // For each screen row [0..sh), we copy pixels from
        //   page row (row + y_off)  if in [0, ph)
        // For each screen col [0..sw), we copy from
        //   page col (col + x_off)  if in [0, pw)
        let x_off = viewport.x_off;
        let y_off = viewport.y_off;

        let row_start_screen = 0i32.max(-y_off);
        let row_end_screen = (sh as i32).min(ph as i32 - y_off);
        let col_start_screen = 0i32.max(-x_off);
        let col_end_screen = (sw as i32).min(pw as i32 - x_off);

        if row_end_screen > row_start_screen && col_end_screen > col_start_screen {
            let page_buf = page.image.as_raw();
            let out_buf = out.as_mut();
            let page_stride = (pw * 4) as usize;
            let out_stride = (sw * 4) as usize;
            let copy_cols = (col_end_screen - col_start_screen) as usize;
            let copy_bytes = copy_cols * 4;

            for row in row_start_screen..row_end_screen {
                let page_row = (row + y_off) as usize;
                let page_col = (col_start_screen + x_off) as usize;
                let src_off = page_row * page_stride + page_col * 4;
                let dst_off = (row as usize) * out_stride + (col_start_screen as usize) * 4;
                out_buf[dst_off..dst_off + copy_bytes]
                    .copy_from_slice(&page_buf[src_off..src_off + copy_bytes]);
            }

            if viewport.night_mode {
                // Invert just the page region we actually copied; pad
                // area is already the correct "night pad" color.
                let out_buf = out.as_mut();
                for row in row_start_screen..row_end_screen {
                    let dst_off = (row as usize) * out_stride + (col_start_screen as usize) * 4;
                    let slice = &mut out_buf[dst_off..dst_off + copy_bytes];
                    for px in slice.chunks_exact_mut(4) {
                        px[0] = 255 - px[0];
                        px[1] = 255 - px[1];
                        px[2] = 255 - px[2];
                        // alpha stays
                    }
                }
            }
        }

        (out, ComposeTiming { compose: start.elapsed() })
    }

    /// Convenience: render_page + compose.
    pub fn render<D: Document>(doc: &D, viewport: &Viewport) -> Result<RenderedFrame> {
        let (page, rt) = Self::render_page(doc, viewport)?;
        let (composed, ct) = Self::compose(&page, viewport);
        Ok(RenderedFrame {
            page,
            composed,
            render: rt.render,
            compose: ct.compose,
        })
    }
}

/// Nearest-neighbour resize for RGBA. We don't need high-quality
/// filtering here — the raster was already done at our chosen
/// raster_scale; this just reconciles quality/dpi deltas.
fn resize_rgba(src: RgbaImage, factor: f32) -> RgbaImage {
    let new_w = ((src.width() as f32) * factor).round().max(1.0) as u32;
    let new_h = ((src.height() as f32) * factor).round().max(1.0) as u32;
    if new_w == src.width() && new_h == src.height() {
        return src;
    }
    image::imageops::resize(&src, new_w, new_h, image::imageops::FilterType::Triangle)
}

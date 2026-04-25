use std::time::Duration;

use anyhow::Result;
use image::RgbaImage;

use crate::cache::CachedPage;
use crate::document::{Document, MatchRect, PageSize, PdfRect};
use crate::viewport::{Rotation, Viewport};

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

/// Search highlights to overlay on the composed image. Rects are in
/// PDF user-space (pre-rotation, pre-scale) — the composer handles the
/// transform. The "current" rect (the one `n`/`N` is sitting on) gets
/// a stronger tint so the user can spot it inside a sea of other hits.
#[derive(Debug, Clone, Default)]
pub struct Highlights {
    pub page_size: Option<PageSize>,
    pub rects: Vec<PdfRect>,
    /// Index into `rects` that should be drawn with the "current" tint
    /// rather than the muted bulk-match tint.
    pub current: Option<usize>,
}

impl Highlights {
    pub fn from_matches(
        matches: &[MatchRect],
        page_idx: usize,
        page_size: PageSize,
        current_global: Option<usize>,
    ) -> Self {
        // Filter the global match list down to this page only and
        // remember which (filtered) entry was the global "current".
        let mut rects = Vec::new();
        let mut current_local: Option<usize> = None;
        for (i, m) in matches.iter().enumerate() {
            if m.page_idx != page_idx {
                continue;
            }
            if Some(i) == current_global {
                current_local = Some(rects.len());
            }
            rects.push(m.rect);
        }
        Self {
            page_size: Some(page_size),
            rects,
            current: current_local,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }
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

        // Nail the bitmap to the dimensions `composed_page_size`
        // predicts — mupdf + our resize both involve rounding, and
        // downstream consumers (Navigator's scroll-range math,
        // compose, the ECache filler) each independently reconstruct
        // the "expected" dimensions. A 1-pixel parity drift between
        // the formulae flips `(page-screen)/2` by one, which pushes
        // x_off off by one, which produces a different EncodedKey —
        // so the filler's pre-encoded frame never matches the paint
        // thread's frame and ECache ends up with duplicate entries
        // per page. Forcing a final resize here is the cheapest way
        // to guarantee everyone agrees on dimensions.
        let (expected_w, expected_h) = viewport.composed_page_size(page_size);
        let image = if image.width() == expected_w && image.height() == expected_h {
            image
        } else {
            image::imageops::resize(
                &image,
                expected_w,
                expected_h,
                image::imageops::FilterType::Triangle,
            )
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
        Self::compose_with_highlights(page, viewport, None)
    }

    /// Same as `compose`, but additionally tints any `Highlights`
    /// rects on top of the composed image. Highlights live in PDF
    /// user-space; we map them through the same rotation + display
    /// scale that produced the rasterised page so the tint sits
    /// exactly over the matched glyph quads.
    pub fn compose_with_highlights(
        page: &CachedPage,
        viewport: &Viewport,
        highlights: Option<&Highlights>,
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

        if let Some(hl) = highlights {
            if let Some(page_size) = hl.page_size {
                draw_highlights(&mut out, viewport, page_size, hl);
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

/// Tint colours used to paint match rectangles. Picked to be readable
/// over both white page bodies and night-mode inverted ones — yellow
/// for inactive hits, orange for the focused (`n`/`N` cursor) hit.
const HIGHLIGHT_TINT: [u8; 3] = [0xFF, 0xE0, 0x40];
const HIGHLIGHT_TINT_CURRENT: [u8; 3] = [0xFF, 0x80, 0x10];
/// Strength of the alpha-blend over the underlying pixel. 0.0 = no
/// tint, 1.0 = solid colour. 0.45 keeps the glyphs legible while still
/// drawing the eye to a hit.
const HIGHLIGHT_ALPHA: f32 = 0.45;
const HIGHLIGHT_ALPHA_CURRENT: f32 = 0.55;

fn draw_highlights(
    out: &mut RgbaImage,
    viewport: &Viewport,
    page_size: PageSize,
    hl: &Highlights,
) {
    if hl.rects.is_empty() {
        return;
    }
    let scale = viewport.display_scale(page_size);
    if scale <= 0.0 {
        return;
    }
    let rotated = viewport.rotation.apply_to_size(page_size);
    let (pw, ph) = viewport.composed_page_size(page_size);
    let sw = out.width() as i32;
    let sh = out.height() as i32;

    for (i, r) in hl.rects.iter().enumerate() {
        let is_current = Some(i) == hl.current;
        let (tint, alpha) = if is_current {
            (HIGHLIGHT_TINT_CURRENT, HIGHLIGHT_ALPHA_CURRENT)
        } else {
            (HIGHLIGHT_TINT, HIGHLIGHT_ALPHA)
        };
        // Map PDF user-space rect through rotation into rotated-page
        // PDF points, then through display_scale into composed-image
        // pixels. Same transform path as `screen_to_pdf_point`,
        // inverted.
        let pts = rotate_pdf_rect(*r, viewport.rotation, page_size, rotated);
        let px0 = (pts.0 * scale).round() as i32 - viewport.x_off;
        let py0 = (pts.1 * scale).round() as i32 - viewport.y_off;
        let px1 = (pts.2 * scale).round() as i32 - viewport.x_off;
        let py1 = (pts.3 * scale).round() as i32 - viewport.y_off;
        let lo_x = px0.min(px1).max(0);
        let lo_y = py0.min(py1).max(0);
        let hi_x = px0.max(px1).min(sw);
        let hi_y = py0.max(py1).min(sh);
        if hi_x <= lo_x || hi_y <= lo_y {
            continue;
        }
        let (_, _) = (pw, ph); // currently unused but kept for clarity
        let stride = out.width() as usize * 4;
        let buf = out.as_mut();
        let inv_a = 1.0 - alpha;
        for y in lo_y..hi_y {
            let row_off = (y as usize) * stride + (lo_x as usize) * 4;
            let row = &mut buf[row_off..row_off + ((hi_x - lo_x) as usize) * 4];
            for px in row.chunks_exact_mut(4) {
                px[0] = ((px[0] as f32) * inv_a + (tint[0] as f32) * alpha) as u8;
                px[1] = ((px[1] as f32) * inv_a + (tint[1] as f32) * alpha) as u8;
                px[2] = ((px[2] as f32) * inv_a + (tint[2] as f32) * alpha) as u8;
                // alpha channel stays opaque
            }
        }
    }
}

/// Apply `rotation` to a PDF user-space rect, returning the four
/// corners as `(x0, y0, x1, y1)` in the rotated page's coordinate
/// system (origin top-left of the rotated page, units PDF points).
fn rotate_pdf_rect(
    r: PdfRect,
    rotation: Rotation,
    page: PageSize,
    rotated: PageSize,
) -> (f32, f32, f32, f32) {
    // Points in the unrotated page.
    let (ax, ay) = (r.x0, r.y0);
    let (bx, by) = (r.x1, r.y1);
    let (cx0, cy0, cx1, cy1) = match rotation {
        Rotation::R0 => (ax, ay, bx, by),
        Rotation::R90 => (page.height - by, ax, page.height - ay, bx),
        Rotation::R180 => (page.width - bx, page.height - by, page.width - ax, page.height - ay),
        Rotation::R270 => (ay, page.width - bx, by, page.width - ax),
    };
    let _ = rotated;
    (cx0, cy0, cx1, cy1)
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

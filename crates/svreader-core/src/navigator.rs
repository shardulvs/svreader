use anyhow::Result;

use crate::document::{Document, PageSize};
use crate::viewport::{Rotation, Viewport, ZoomMode};

/// Fraction of the screen kept visible when a `j`/`k` scroll crosses
/// a page boundary. Matches koreader's ~10% default.
pub const SCROLL_OVERLAP: f32 = 0.10;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    NextScreen,
    PrevScreen,
    NextPage,
    PrevPage,
    HalfScreenDown,
    HalfScreenUp,
    ScrollLeft,
    ScrollRight,
    PageTop,
    PageMiddle,
    PageBottom,
    FirstPage,
    LastPage,
    GotoPage(usize),

    SetZoom(ZoomMode),
    ZoomBy(f32),
    RotateCw,
    RotateCcw,
    SetRotation(Rotation),

    SetNight(bool),
    ToggleNight,

    SetRenderDpi(Option<f32>),
    SetRenderQuality(f32),

    Resize(u32, u32),

    /// No-op. Convenient for Esc, repeated stateless keys, etc.
    None,
}

pub struct Navigator;

impl Navigator {
    /// Apply an action against a document and mutate `viewport`.
    /// Pure state transition — no rendering happens here.
    pub fn apply<D: Document>(
        doc: &D,
        viewport: &mut Viewport,
        action: Action,
    ) -> Result<()> {
        match action {
            Action::None => {}
            Action::Resize(w, h) => {
                viewport.screen_w = w.max(1);
                viewport.screen_h = h.max(1);
                let size = doc.page_size(viewport.page_idx)?;
                // Zoom re-derives from screen size; reset offsets so
                // a resize doesn't leave us scrolled off the page.
                snap_to_zoom_anchor(viewport, size);
            }

            Action::NextScreen => next_screen(doc, viewport)?,
            Action::PrevScreen => prev_screen(doc, viewport)?,
            Action::NextPage => goto_page(doc, viewport, viewport.page_idx + 1, Anchor::Start)?,
            Action::PrevPage => {
                if viewport.page_idx == 0 {
                    goto_page(doc, viewport, 0, Anchor::Start)?;
                } else {
                    goto_page(doc, viewport, viewport.page_idx - 1, Anchor::Start)?;
                }
            }
            Action::HalfScreenDown => half_screen(doc, viewport, 1)?,
            Action::HalfScreenUp => half_screen(doc, viewport, -1)?,
            Action::ScrollLeft => scroll_horiz(doc, viewport, -1)?,
            Action::ScrollRight => scroll_horiz(doc, viewport, 1)?,

            Action::PageTop => goto_anchor(doc, viewport, Anchor::Start)?,
            Action::PageMiddle => goto_anchor(doc, viewport, Anchor::Middle)?,
            Action::PageBottom => goto_anchor(doc, viewport, Anchor::End)?,

            Action::FirstPage => goto_page(doc, viewport, 0, Anchor::Start)?,
            Action::LastPage => {
                let last = doc.page_count().saturating_sub(1);
                goto_page(doc, viewport, last, Anchor::Start)?;
            }
            Action::GotoPage(idx) => goto_page(doc, viewport, idx, Anchor::Start)?,

            Action::SetZoom(z) => {
                viewport.zoom = z;
                let size = doc.page_size(viewport.page_idx)?;
                snap_to_zoom_anchor(viewport, size);
            }
            Action::ZoomBy(f) => {
                if f > 0.0 {
                    let current_mult = match viewport.zoom {
                        ZoomMode::Custom(m) => m,
                        _ => 1.0,
                    };
                    viewport.zoom = ZoomMode::Custom((current_mult * f).clamp(0.1, 10.0));
                    let size = doc.page_size(viewport.page_idx)?;
                    snap_to_zoom_anchor(viewport, size);
                }
            }
            Action::RotateCw => {
                viewport.rotation = viewport.rotation.cw();
                let size = doc.page_size(viewport.page_idx)?;
                snap_to_zoom_anchor(viewport, size);
            }
            Action::RotateCcw => {
                viewport.rotation = viewport.rotation.ccw();
                let size = doc.page_size(viewport.page_idx)?;
                snap_to_zoom_anchor(viewport, size);
            }
            Action::SetRotation(r) => {
                viewport.rotation = r;
                let size = doc.page_size(viewport.page_idx)?;
                snap_to_zoom_anchor(viewport, size);
            }

            Action::SetNight(b) => viewport.night_mode = b,
            Action::ToggleNight => viewport.night_mode = !viewport.night_mode,

            Action::SetRenderDpi(dpi) => viewport.render_dpi = dpi,
            Action::SetRenderQuality(q) => viewport.render_quality = q.clamp(0.1, 2.0),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Anchor {
    Start,
    Middle,
    End,
}

fn composed_or_zero<D: Document>(doc: &D, viewport: &Viewport) -> Result<(u32, u32, PageSize)> {
    let size = doc.page_size(viewport.page_idx)?;
    let (w, h) = viewport.composed_page_size(size);
    Ok((w, h, size))
}

fn snap_to_zoom_anchor(viewport: &mut Viewport, size: PageSize) {
    // After any zoom/rotation change, pin to the top-left of the page
    // content (or the centered offset for dimensions that fit).
    let (pw, ph) = viewport.composed_page_size(size);
    let (xmin, _) = viewport.x_range(pw);
    let (ymin, _) = viewport.y_range(ph);
    viewport.x_off = xmin;
    viewport.y_off = ymin;
}

fn next_screen<D: Document>(doc: &D, viewport: &mut Viewport) -> Result<()> {
    let (pw, ph, size) = composed_or_zero(doc, viewport)?;
    let (_, ymax) = viewport.y_range(ph);
    let fits = viewport.page_fits(size);
    if fits {
        // Whole page visible → next page.
        if viewport.page_idx + 1 < doc.page_count() {
            goto_page(doc, viewport, viewport.page_idx + 1, Anchor::Start)?;
        }
        return Ok(());
    }
    if viewport.y_off >= ymax {
        // At bottom of page → move to next page.
        if viewport.page_idx + 1 < doc.page_count() {
            goto_page(doc, viewport, viewport.page_idx + 1, Anchor::Start)?;
        }
        return Ok(());
    }
    let step = ((viewport.screen_h as f32) * (1.0 - SCROLL_OVERLAP)).round() as i32;
    let step = step.max(1);
    viewport.y_off = (viewport.y_off + step).min(ymax);
    // keep x clamped (may change if zoom changes x fit-status)
    let _ = pw;
    Ok(())
}

fn prev_screen<D: Document>(doc: &D, viewport: &mut Viewport) -> Result<()> {
    let (_, ph, size) = composed_or_zero(doc, viewport)?;
    let (ymin, _) = viewport.y_range(ph);
    let fits = viewport.page_fits(size);
    if fits {
        if viewport.page_idx > 0 {
            goto_page(doc, viewport, viewport.page_idx - 1, Anchor::End)?;
        }
        return Ok(());
    }
    if viewport.y_off <= ymin {
        if viewport.page_idx > 0 {
            goto_page(doc, viewport, viewport.page_idx - 1, Anchor::End)?;
        }
        return Ok(());
    }
    let step = ((viewport.screen_h as f32) * (1.0 - SCROLL_OVERLAP)).round() as i32;
    let step = step.max(1);
    viewport.y_off = (viewport.y_off - step).max(ymin);
    Ok(())
}

fn half_screen<D: Document>(doc: &D, viewport: &mut Viewport, dir: i32) -> Result<()> {
    let (_, ph, size) = composed_or_zero(doc, viewport)?;
    let (ymin, ymax) = viewport.y_range(ph);
    if viewport.page_fits(size) {
        // No vertical scroll room: half-page acts like next/prev page.
        if dir > 0 && viewport.page_idx + 1 < doc.page_count() {
            goto_page(doc, viewport, viewport.page_idx + 1, Anchor::Start)?;
        } else if dir < 0 && viewport.page_idx > 0 {
            goto_page(doc, viewport, viewport.page_idx - 1, Anchor::Start)?;
        }
        return Ok(());
    }
    let step = (viewport.screen_h as i32 / 2).max(1);
    let target = viewport.y_off + dir * step;
    if target > ymax && dir > 0 && viewport.page_idx + 1 < doc.page_count() {
        goto_page(doc, viewport, viewport.page_idx + 1, Anchor::Start)?;
    } else if target < ymin && dir < 0 && viewport.page_idx > 0 {
        goto_page(doc, viewport, viewport.page_idx - 1, Anchor::End)?;
    } else {
        viewport.y_off = target.clamp(ymin, ymax);
    }
    Ok(())
}

fn scroll_horiz<D: Document>(doc: &D, viewport: &mut Viewport, dir: i32) -> Result<()> {
    let (pw, _, _) = composed_or_zero(doc, viewport)?;
    let (xmin, xmax) = viewport.x_range(pw);
    let step = ((viewport.screen_w as f32) * (1.0 - SCROLL_OVERLAP)).round() as i32;
    let step = step.max(1);
    viewport.x_off = (viewport.x_off + dir * step).clamp(xmin, xmax);
    Ok(())
}

fn goto_anchor<D: Document>(doc: &D, viewport: &mut Viewport, anchor: Anchor) -> Result<()> {
    let size = doc.page_size(viewport.page_idx)?;
    apply_anchor(viewport, size, anchor);
    Ok(())
}

fn goto_page<D: Document>(
    doc: &D,
    viewport: &mut Viewport,
    page_idx: usize,
    anchor: Anchor,
) -> Result<()> {
    let count = doc.page_count();
    if count == 0 {
        return Ok(());
    }
    let clamped = page_idx.min(count - 1);
    viewport.page_idx = clamped;
    let size = doc.page_size(clamped)?;
    apply_anchor(viewport, size, anchor);
    Ok(())
}

fn apply_anchor(viewport: &mut Viewport, size: PageSize, anchor: Anchor) {
    let (pw, ph) = viewport.composed_page_size(size);
    let (xmin, xmax) = viewport.x_range(pw);
    let (ymin, ymax) = viewport.y_range(ph);
    // Keep current x_off if in range, else snap to xmin.
    viewport.x_off = viewport.x_off.clamp(xmin, xmax);
    match anchor {
        Anchor::Start => viewport.y_off = ymin,
        Anchor::End => viewport.y_off = ymax,
        Anchor::Middle => viewport.y_off = (ymin + ymax) / 2,
    }
    let _ = Rotation::R0;
}

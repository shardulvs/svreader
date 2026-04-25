use serde::{Deserialize, Serialize};

use crate::document::PageSize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ZoomMode {
    FitWidth,
    FitHeight,
    FitPage,
    /// Multiplier on native page size. 1.0 = 100%.
    Custom(f32),
}

impl ZoomMode {
    pub fn label(self) -> String {
        match self {
            ZoomMode::FitWidth => "fit-w".into(),
            ZoomMode::FitHeight => "fit-h".into(),
            ZoomMode::FitPage => "fit-p".into(),
            ZoomMode::Custom(m) => format!("{:.0}%", m * 100.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Rotation {
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub fn degrees(self) -> u32 {
        match self {
            Rotation::R0 => 0,
            Rotation::R90 => 90,
            Rotation::R180 => 180,
            Rotation::R270 => 270,
        }
    }

    pub fn from_degrees(deg: i32) -> Self {
        match deg.rem_euclid(360) {
            0 => Rotation::R0,
            90 => Rotation::R90,
            180 => Rotation::R180,
            270 => Rotation::R270,
            // Snap weird values to the nearest quadrant.
            d if d < 90 => Rotation::R0,
            d if d < 180 => Rotation::R90,
            d if d < 270 => Rotation::R180,
            _ => Rotation::R270,
        }
    }

    pub fn cw(self) -> Self {
        Rotation::from_degrees(self.degrees() as i32 + 90)
    }

    pub fn ccw(self) -> Self {
        Rotation::from_degrees(self.degrees() as i32 - 90)
    }

    /// Returns the page size as seen after rotation.
    pub fn apply_to_size(self, size: PageSize) -> PageSize {
        match self {
            Rotation::R0 | Rotation::R180 => size,
            Rotation::R90 | Rotation::R270 => PageSize {
                width: size.height,
                height: size.width,
            },
        }
    }
}

/// View state that drives rendering. Purely data — no I/O.
#[derive(Debug, Clone)]
pub struct Viewport {
    pub page_idx: usize,
    /// Top-left scroll offset in composed-image pixels. May be negative
    /// when the page is smaller than the screen along that axis, which
    /// centers the page inside the screen with padding around it.
    pub x_off: i32,
    pub y_off: i32,
    pub zoom: ZoomMode,
    pub rotation: Rotation,
    /// Screen pixel dims of the image we're composing into.
    pub screen_w: u32,
    pub screen_h: u32,
    pub night_mode: bool,
    /// Raster DPI override. `None` means "rasterise at display scale".
    pub render_dpi: Option<f32>,
    /// Quality knob: 0.1..2.0 multiplier on the transmitted image
    /// density. Doesn't affect layout math; it only scales the image
    /// we send.
    pub render_quality: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            page_idx: 0,
            x_off: 0,
            y_off: 0,
            zoom: ZoomMode::FitWidth,
            rotation: Rotation::R0,
            screen_w: 800,
            screen_h: 600,
            night_mode: false,
            render_dpi: None,
            render_quality: 1.0,
        }
    }
}

impl Viewport {
    /// Fit-to-screen scale factor, in screen-pixels-per-page-point,
    /// ignoring quality and dpi overrides.
    ///
    /// `page_size` is the PDF-native page size (pre-rotation).
    pub fn display_scale(&self, page_size: PageSize) -> f32 {
        let rotated = self.rotation.apply_to_size(page_size);
        if rotated.width <= 0.0 || rotated.height <= 0.0 {
            return 1.0;
        }
        let screen_w = self.screen_w.max(1) as f32;
        let screen_h = self.screen_h.max(1) as f32;
        match self.zoom {
            ZoomMode::FitWidth => screen_w / rotated.width,
            ZoomMode::FitHeight => screen_h / rotated.height,
            ZoomMode::FitPage => (screen_w / rotated.width).min(screen_h / rotated.height),
            ZoomMode::Custom(mult) => {
                // 1.0 = 100% native at 72 DPI. Map native points to
                // screen pixels such that a 100% page at the default
                // display scale fills ~fit-width.
                // We treat Custom as "fit-width × mult" so navigation
                // feels sensible ("100%" == fit-width, 200% == 2x).
                (screen_w / rotated.width) * mult.max(0.01)
            }
        }
    }

    /// Raster scale applied to mupdf. This is the scale at which we
    /// ask mupdf to draw the page. Combines display scale, render_dpi
    /// override, and quality.
    pub fn raster_scale(&self, page_size: PageSize) -> f32 {
        let disp = self.display_scale(page_size);
        let dpi_factor = match self.render_dpi {
            Some(dpi) => dpi / 72.0,
            None => disp,
        };
        (dpi_factor * self.render_quality.max(0.05)).max(0.05)
    }

    /// Effective raster DPI (for display in the status bar).
    pub fn effective_dpi(&self, page_size: PageSize) -> f32 {
        match self.render_dpi {
            Some(dpi) => dpi,
            None => self.display_scale(page_size) * 72.0,
        }
    }

    /// Composed page dimensions in screen pixels (post-rotation and
    /// display-scale, pre-quality — quality only affects the sixel
    /// encode, not the compose buffer).
    pub fn composed_page_size(&self, page_size: PageSize) -> (u32, u32) {
        let rotated = self.rotation.apply_to_size(page_size);
        let scale = self.display_scale(page_size);
        let w = (rotated.width * scale).round().max(1.0) as u32;
        let h = (rotated.height * scale).round().max(1.0) as u32;
        (w, h)
    }

    /// Horizontal scroll bounds: (min_off, max_off).
    ///
    /// If the page fits along x, returns a collapsed range whose only
    /// value is the centered (negative) offset, so narrow pages sit
    /// centred with padding.
    pub fn x_range(&self, page_w: u32) -> (i32, i32) {
        let screen = self.screen_w as i32;
        let page = page_w as i32;
        if page <= screen {
            let off = (page - screen) / 2; // negative
            (off, off)
        } else {
            (0, page - screen)
        }
    }

    pub fn y_range(&self, page_h: u32) -> (i32, i32) {
        let screen = self.screen_h as i32;
        let page = page_h as i32;
        if page <= screen {
            let off = (page - screen) / 2; // negative
            (off, off)
        } else {
            (0, page - screen)
        }
    }

    /// Whether the page fits entirely inside the screen (both axes).
    pub fn page_fits(&self, page_size: PageSize) -> bool {
        let (pw, ph) = self.composed_page_size(page_size);
        pw <= self.screen_w && ph <= self.screen_h
    }

    /// Clamp current offsets into their scroll ranges for a page.
    pub fn clamp_offsets(&mut self, page_size: PageSize) {
        let (pw, ph) = self.composed_page_size(page_size);
        let (xmin, xmax) = self.x_range(pw);
        let (ymin, ymax) = self.y_range(ph);
        self.x_off = self.x_off.clamp(xmin, xmax);
        self.y_off = self.y_off.clamp(ymin, ymax);
    }

    /// Convert a screen pixel inside this viewport (relative to the
    /// window's top-left) to a point on the current page in PDF
    /// user-space points (pre-rotation, pre-scale). Returns None if
    /// the click landed in the padding area outside the page.
    pub fn screen_to_pdf_point(
        &self,
        page_size: PageSize,
        screen_x: i32,
        screen_y: i32,
    ) -> Option<(f32, f32)> {
        let scale = self.display_scale(page_size);
        if scale <= 0.0 {
            return None;
        }
        let (pw, ph) = self.composed_page_size(page_size);
        // Position on the composed (rotated) page in screen pixels.
        let px = screen_x + self.x_off;
        let py = screen_y + self.y_off;
        if px < 0 || py < 0 || px >= pw as i32 || py >= ph as i32 {
            return None;
        }
        let pxf = px as f32;
        let pyf = py as f32;
        let rotated = self.rotation.apply_to_size(page_size);
        // Convert rotated-page screen pixels back to rotated-page PDF
        // points (divide out display_scale).
        let rx = pxf / scale;
        let ry = pyf / scale;
        // Then unrotate back to the page's native coordinate system.
        let (ux, uy) = match self.rotation {
            Rotation::R0 => (rx, ry),
            Rotation::R90 => (ry, rotated.width - rx),
            Rotation::R180 => (rotated.width - rx, rotated.height - ry),
            Rotation::R270 => (rotated.height - ry, rx),
        };
        Some((ux, uy))
    }
}

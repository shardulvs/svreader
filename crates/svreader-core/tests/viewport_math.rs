use svreader_core::document::PageSize;
use svreader_core::{Rotation, Viewport, ZoomMode};

fn viewport(w: u32, h: u32, zoom: ZoomMode) -> Viewport {
    Viewport {
        screen_w: w,
        screen_h: h,
        zoom,
        ..Viewport::default()
    }
}

#[test]
fn fit_width_scales_to_screen_width() {
    let v = viewport(600, 400, ZoomMode::FitWidth);
    let ps = PageSize {
        width: 300.0,
        height: 400.0,
    };
    let s = v.display_scale(ps);
    assert!((s - 2.0).abs() < 1e-4);
}

#[test]
fn fit_page_uses_smaller_axis() {
    let v = viewport(600, 400, ZoomMode::FitPage);
    let ps = PageSize {
        width: 100.0,
        height: 400.0,
    };
    let s = v.display_scale(ps);
    // Page is tall relative to screen: fit-page == height/400 == 1.0.
    assert!((s - 1.0).abs() < 1e-4);
}

#[test]
fn rotation_swaps_axes() {
    let v = viewport(600, 400, ZoomMode::FitWidth);
    let ps = PageSize {
        width: 300.0,
        height: 900.0,
    };
    let s0 = v.display_scale(ps);
    let v90 = Viewport {
        rotation: Rotation::R90,
        ..v.clone()
    };
    let s90 = v90.display_scale(ps);
    // Rotated 90° → width is now 900, fit-w would be 600/900 ≠ 600/300
    assert!((s0 - 2.0).abs() < 1e-4);
    assert!((s90 - (600.0 / 900.0)).abs() < 1e-4);
}

#[test]
fn narrow_page_gets_centered_offsets() {
    // FitPage on a 600x400 screen for a 200x200 page → scaled to
    // (400,400) which is narrower than screen on x and equal on y.
    let v = viewport(600, 400, ZoomMode::FitPage);
    let ps = PageSize {
        width: 200.0,
        height: 200.0,
    };
    let (pw, ph) = v.composed_page_size(ps);
    assert!(pw <= 600);
    assert!(ph <= 400);
    let (xmin, xmax) = v.x_range(pw);
    let (ymin, ymax) = v.y_range(ph);
    // Both axes fit → range collapses to a single centered offset.
    assert_eq!(xmin, xmax);
    assert_eq!(ymin, ymax);
    // And x is strictly inside (page is narrower than screen).
    assert!(xmin < 0, "narrow page should produce negative x offset");
    // ymin <= 0 (could be 0 if page height exactly fills screen).
    assert!(ymin <= 0);
}

#[test]
fn quality_and_dpi_change_raster_scale_only() {
    let ps = PageSize {
        width: 600.0,
        height: 800.0,
    };
    let mut v = viewport(600, 400, ZoomMode::FitWidth);
    let base_disp = v.display_scale(ps);
    let base_rs = v.raster_scale(ps);
    assert!((base_disp - base_rs).abs() < 1e-4);

    v.render_dpi = Some(144.0);
    assert!((v.display_scale(ps) - base_disp).abs() < 1e-4);
    let rs = v.raster_scale(ps);
    assert!(rs > base_rs, "dpi 144 should raster at a higher scale");

    v.render_dpi = None;
    v.render_quality = 0.5;
    let rs_q = v.raster_scale(ps);
    assert!(rs_q < base_rs);
}

#[test]
fn effective_dpi_reports_72_times_scale() {
    let ps = PageSize {
        width: 300.0,
        height: 400.0,
    };
    let v = viewport(600, 400, ZoomMode::FitWidth);
    let dpi = v.effective_dpi(ps);
    // fit-w scale = 2.0, so effective DPI = 144.
    assert!((dpi - 144.0).abs() < 1e-3);
}

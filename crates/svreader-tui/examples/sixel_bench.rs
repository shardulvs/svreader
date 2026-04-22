//! Microbench for the sixel encode path. Uses real PDF output so
//! numbers reflect actual text-on-white content, not synthetic noise.
//!
//! Run with:
//!   cargo run --release -p svreader-tui --example sixel_bench -- PATH/TO/file.pdf

use std::env;
use std::io::sink;
use std::time::Instant;

use svreader_core::{Renderer, Viewport};
use svreader_core::pdf::PdfDocument;
use svreader_tui::bench::{encode_and_write_bench, ColorMode};

fn main() -> anyhow::Result<()> {
    let pdf = env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: sixel_bench path/to.pdf [WxH]"))?;
    let size = env::args().nth(2).unwrap_or_else(|| "1920x1080".into());
    let (w, h) = size
        .split_once('x')
        .map(|(a, b)| (a.parse::<u32>().unwrap(), b.parse::<u32>().unwrap()))
        .unwrap();

    let doc = PdfDocument::open(&pdf)?;
    let mut vp = Viewport::default();
    vp.screen_w = w;
    vp.screen_h = h;

    for page in 0..3.min(svreader_core::document::Document::page_count(&doc) as i32) {
        vp.page_idx = page as usize;
        let frame = Renderer::render(&doc, &vp)?;
        let img = frame.composed;
        for mode in [ColorMode::XTerm256, ColorMode::Grayscale] {
            let mut best = std::time::Duration::MAX;
            for _ in 0..3 {
                let t0 = Instant::now();
                encode_and_write_bench(&img, 0, 0, mode, &mut sink())?;
                let dt = t0.elapsed();
                if dt < best {
                    best = dt;
                }
            }
            eprintln!(
                "page {} size {}x{}  mode {:?}  best {:?}",
                page + 1,
                img.width(),
                img.height(),
                mode,
                best
            );
        }
    }
    Ok(())
}

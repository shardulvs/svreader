use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use image::RgbaImage;
use mupdf::{Colorspace, Document as MuDocument, Matrix, TextPageFlags};

use crate::document::{Document, Outline, PageMetrics, PageSize};
use crate::viewport::Rotation;

/// mupdf-backed PDF. `mupdf` handles are not `Send`/`Sync`, so every
/// thread that wants to render holds its own `PdfDocument`.
pub struct PdfDocument {
    path: PathBuf,
    inner: MuDocument,
}

impl PdfDocument {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("path {:?} is not valid UTF-8", path))?;
        let inner = MuDocument::open(path_str)
            .with_context(|| format!("failed to open PDF {:?}", path))?;
        if inner
            .needs_password()
            .context("failed to check if PDF is password-protected")?
        {
            anyhow::bail!("PDF {:?} is password-protected (unsupported)", path);
        }
        Ok(Self {
            path: path.to_path_buf(),
            inner,
        })
    }

    /// Re-open the same file into a fresh document — used by worker
    /// threads that need their own mupdf state.
    pub fn reopen(&self) -> Result<Self> {
        Self::open(&self.path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PageMetrics for PdfDocument {
    fn page_count(&self) -> usize {
        self.inner.page_count().unwrap_or(0).max(0) as usize
    }

    fn page_size(&self, page_idx: usize) -> Result<PageSize> {
        let page = self
            .inner
            .load_page(page_idx as i32)
            .with_context(|| format!("failed to load page {page_idx}"))?;
        let bounds = page.bounds().context("failed to compute page bounds")?;
        Ok(PageSize {
            width: (bounds.x1 - bounds.x0).max(1.0),
            height: (bounds.y1 - bounds.y0).max(1.0),
        })
    }
}

impl Document for PdfDocument {
    fn render_page(
        &self,
        page_idx: usize,
        scale: f32,
        rotation: Rotation,
    ) -> Result<RgbaImage> {
        let page = self
            .inner
            .load_page(page_idx as i32)
            .with_context(|| format!("failed to load page {page_idx}"))?;

        let mut ctm = Matrix::new_scale(scale, scale);
        if rotation != Rotation::R0 {
            ctm.rotate(rotation.degrees() as f32);
        }

        let cs = Colorspace::device_rgb();
        let pixmap = page
            .to_pixmap(&ctm, &cs, false, true)
            .context("mupdf failed to rasterise page")?;

        let w = pixmap.width();
        let h = pixmap.height();
        let n = pixmap.n() as usize; // samples per pixel
        let samples = pixmap.samples();
        let stride = pixmap.stride() as usize;

        let mut out = RgbaImage::new(w, h);
        let out_raw = out.as_mut();
        for y in 0..h as usize {
            let src_row = &samples[y * stride..y * stride + w as usize * n];
            let dst_row = &mut out_raw[y * w as usize * 4..(y + 1) * w as usize * 4];
            match n {
                3 => {
                    for x in 0..w as usize {
                        let s = &src_row[x * 3..x * 3 + 3];
                        let d = &mut dst_row[x * 4..x * 4 + 4];
                        d[0] = s[0];
                        d[1] = s[1];
                        d[2] = s[2];
                        d[3] = 255;
                    }
                }
                4 => {
                    dst_row.copy_from_slice(src_row);
                }
                1 => {
                    for x in 0..w as usize {
                        let v = src_row[x];
                        let d = &mut dst_row[x * 4..x * 4 + 4];
                        d[0] = v;
                        d[1] = v;
                        d[2] = v;
                        d[3] = 255;
                    }
                }
                _ => {
                    anyhow::bail!("unexpected mupdf pixmap component count: {}", n);
                }
            }
        }
        Ok(out)
    }

    fn outline(&self) -> Result<Vec<Outline>> {
        let raw = self
            .inner
            .outlines()
            .context("failed to read PDF outline")?;
        Ok(convert_outlines(&raw))
    }

    fn page_text(&self, page_idx: usize) -> Result<String> {
        let page = self
            .inner
            .load_page(page_idx as i32)
            .with_context(|| format!("failed to load page {page_idx}"))?;
        let tp = page
            .to_text_page(TextPageFlags::empty())
            .context("failed to extract text page")?;
        Ok(tp.to_text().context("failed to stringify text page")?)
    }
}

fn convert_outlines(raw: &[mupdf::outline::Outline]) -> Vec<Outline> {
    raw.iter()
        .map(|o| Outline {
            title: o.title.clone(),
            page: o.dest.as_ref().map(|d| d.loc.page_number as usize),
            children: convert_outlines(&o.down),
        })
        .collect()
}

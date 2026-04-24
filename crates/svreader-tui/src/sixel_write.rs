//! Sixel encoding + stdout emission.
//!
//! Uses libsixel via a thin direct binding so we can pick a built-in
//! palette (fast path) instead of the default adaptive 256-color
//! quantisation. `sixel-bytes`' convenience API ran ~160ms per frame
//! at 1920x1080; with `BuiltinDither::G8` for grayscale / XTerm256
//! for colour, we reach ~10-30ms.
//!
//! The encode and the stdout write are split into two public entry
//! points (`encode_rgba_to_dcs` and `emit_dcs`) so the encoded DCS
//! payload can be cached by `encoded_cache::ComposedEncodedCache`
//! and re-emitted without re-encoding.

use std::ffi::{c_int, c_uchar, c_void};
use std::io::{self, Write};
use std::os::raw::c_char;
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use image::RgbaImage;
use parking_lot::Mutex;
use sixel_sys_static::{
    sixel_dither_get, sixel_dither_set_diffusion_type, sixel_dither_set_pixelformat,
    sixel_encode, sixel_output_destroy, sixel_output_new, sixel_output_set_encode_policy,
    BuiltinDither, DiffusionMethod, Dither, EncodePolicy, Output, PixelFormat,
};

use crate::tmux::wrap_for_tmux;

/// libsixel's `BuiltinDither` is a process-wide singleton, and we
/// reconfigure its pixel format + diffusion policy before every
/// encode. Two threads racing on that is unsafe. The ECache filler
/// runs on a background thread, so this mutex serialises every
/// encode, ensuring at most one is in flight at a time.
fn encode_mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

pub struct SixelEmitTiming {
    pub encode: Duration,
    pub write: Duration,
    #[allow(dead_code)]
    pub bytes: usize,
}

/// Colour mode for sixel output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorMode {
    /// 256-colour xterm palette — good default for mixed content.
    XTerm256,
    /// 8-bit grayscale palette — fastest for text-only pages.
    Grayscale,
}

impl ColorMode {
    fn to_builtin(self) -> BuiltinDither {
        match self {
            ColorMode::XTerm256 => BuiltinDither::XTerm256,
            ColorMode::Grayscale => BuiltinDither::G8,
        }
    }

    /// Stable 1-byte tag for use in cache keys.
    pub fn tag(self) -> u8 {
        match self {
            ColorMode::XTerm256 => 0,
            ColorMode::Grayscale => 1,
        }
    }
}

/// Encode an RGBA image to a sixel DCS string using a built-in
/// palette. Returns the DCS bytes plus the time spent encoding.
pub fn encode_rgba_to_dcs(image: &RgbaImage, mode: ColorMode) -> Result<(String, Duration)> {
    let w = image.width() as i32;
    let h = image.height() as i32;
    let t0 = Instant::now();
    let dcs = encode_rgba(image.as_raw(), w, h, mode)?;
    Ok((dcs, t0.elapsed()))
}

/// Write an already-encoded sixel DCS string to `out`, positioning
/// the cursor at `(col, row)` first. Returns the time spent writing
/// and the byte count placed on the wire.
pub fn emit_dcs(
    dcs: &str,
    col: u16,
    row: u16,
    out: &mut impl Write,
) -> Result<(Duration, usize)> {
    let t1 = Instant::now();
    write!(out, "\x1b[{};{}H", row + 1, col + 1)?;
    let payload = wrap_for_tmux(dcs);
    out.write_all(payload.as_bytes())?;
    out.flush()?;
    Ok((t1.elapsed(), payload.len()))
}

/// Convenience: encode + emit. Callers that cache the DCS payload
/// use `encode_rgba_to_dcs` and `emit_dcs` separately so the encode
/// step can be skipped on a cache hit.
pub fn encode_and_write(
    image: &RgbaImage,
    col: u16,
    row: u16,
    mode: ColorMode,
    out: &mut impl Write,
) -> Result<SixelEmitTiming> {
    let (dcs, encode) = encode_rgba_to_dcs(image, mode)?;
    let (write, bytes) = emit_dcs(&dcs, col, row, out)?;
    Ok(SixelEmitTiming { encode, write, bytes })
}

/// Encode RGBA to a sixel DCS string using a built-in palette.
fn encode_rgba(bytes: &[u8], width: i32, height: i32, mode: ColorMode) -> Result<String> {
    if width <= 0 || height <= 0 {
        return Err(anyhow!("bad sixel dims {width}x{height}"));
    }
    let expected = (width * height * 4) as usize;
    if bytes.len() != expected {
        return Err(anyhow!(
            "rgba buffer size {} != expected {}",
            bytes.len(),
            expected
        ));
    }

    // Serialise every call into libsixel — see `encode_mutex`.
    let _guard = encode_mutex().lock();

    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let buf_ptr: *mut c_void = &mut buf as *mut _ as *mut c_void;

    let mut output: *mut Output = ptr::null_mut();
    let dither: *mut Dither;

    unsafe extern "C" fn write_cb(
        data: *mut c_char,
        size: c_int,
        user: *mut c_void,
    ) -> c_int {
        if data.is_null() || size <= 0 {
            return 0;
        }
        let v: &mut Vec<u8> = &mut *(user as *mut Vec<u8>);
        let slice = std::slice::from_raw_parts(data as *const u8, size as usize);
        v.extend_from_slice(slice);
        0 // SIXEL_OK
    }

    unsafe {
        let rc = sixel_output_new(&mut output, Some(write_cb), buf_ptr, ptr::null_mut());
        check(rc, "sixel_output_new")?;
        sixel_output_set_encode_policy(output, EncodePolicy::Fast);

        dither = sixel_dither_get(mode.to_builtin());
        if dither.is_null() {
            sixel_output_destroy(output);
            return Err(anyhow!("sixel_dither_get returned null"));
        }
        sixel_dither_set_pixelformat(dither, PixelFormat::RGBA8888);
        // No error diffusion: ~3-4× faster and still looks fine for
        // text and anti-aliased line art, which is what PDFs are.
        sixel_dither_set_diffusion_type(dither, DiffusionMethod::None);

        let rc = sixel_encode(
            bytes.as_ptr() as *mut c_uchar,
            width,
            height,
            8, // depth ignored by libsixel
            dither,
            output,
        );
        let enc_rc = rc;
        // Built-in dither returned by sixel_dither_get must NOT be
        // destroyed (shared singleton).
        sixel_output_destroy(output);
        let _ = dither;
        check(enc_rc, "sixel_encode")?;
    }

    String::from_utf8(buf).map_err(|e| anyhow!("sixel output not utf8: {e}"))
}

fn check(status: c_int, ctx: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(anyhow!("{ctx} failed with status {status}"))
    }
}

/// Blank a rectangle bounded by `(col, row)` with `cols` × `rows`
/// cells. Writes spaces into each affected row rather than using
/// `\x1b[2K` (which clears the entire row) so neighbouring windows
/// survive the clear.
pub fn blank_rect(
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    out: &mut impl Write,
) -> io::Result<()> {
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    let blanks = " ".repeat(cols as usize);
    for r in 0..rows {
        write!(out, "\x1b[{};{}H{}", row + r + 1, col + 1, blanks)?;
    }
    out.flush()
}

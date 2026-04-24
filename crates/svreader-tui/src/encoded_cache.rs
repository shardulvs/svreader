//! LRU of fully-composed, fully-encoded sixel DCS payloads.
//!
//! Sits on top of `svreader_core::RenderCache`. The render cache
//! stores rasterised page bitmaps keyed on `(buffer, page,
//! display_scale, raster_scale, rotation)`; the encoded cache stores
//! the entire composed-and-encoded frame keyed on every input that
//! affects the final sixel bytes — screen dims, scroll offsets,
//! night mode, and the palette (`ColorMode`) too.
//!
//! Encoding a large page to sixel dominates the frame budget
//! (~100ms). Caching the encoded bytes turns re-display of a
//! previously-viewed (page, viewport, palette) combination into an
//! O(write) operation: push the cached DCS string to stdout and
//! we're done.
//!
//! Races with the render cache: the paint thread asks the encoded
//! cache for an entry; on a miss it calls into the render cache
//! (which single-flights concurrent renders for the same page), then
//! composes and encodes. The encoded cache has its own single-flight
//! so two paint calls for the same fully-keyed viewport don't
//! duplicate the 100ms encode either.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use lru::LruCache;
use parking_lot::{Condvar, Mutex};
use svreader_core::{BufferId, Rotation, Viewport};

use crate::sixel_write::ColorMode;

/// Fully-qualified key for a composed + encoded frame. Every input
/// that feeds `Renderer::compose` and `encode_rgba_to_dcs` is in the
/// key — otherwise a stale entry could leak into a different render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EncodedKey {
    pub buffer: BufferId,
    pub page_idx: usize,
    pub display_scale_q: u32,
    pub raster_scale_q: u32,
    pub rotation: Rotation,
    pub screen_w: u32,
    pub screen_h: u32,
    pub x_off: i32,
    pub y_off: i32,
    pub night_mode: bool,
    pub color_mode: u8,
}

impl EncodedKey {
    pub fn from_viewport(
        buffer: BufferId,
        viewport: &Viewport,
        display_scale: f32,
        raster_scale: f32,
        color_mode: ColorMode,
    ) -> Self {
        Self {
            buffer,
            page_idx: viewport.page_idx,
            display_scale_q: (display_scale * 256.0).round().max(0.0) as u32,
            raster_scale_q: (raster_scale * 256.0).round().max(0.0) as u32,
            rotation: viewport.rotation,
            screen_w: viewport.screen_w,
            screen_h: viewport.screen_h,
            x_off: viewport.x_off,
            y_off: viewport.y_off,
            night_mode: viewport.night_mode,
            color_mode: color_mode.tag(),
        }
    }
}

/// Cached encoded frame. `dcs` is the raw DCS string — tmux wrapping
/// is applied at emit time (it depends on environment, not on the
/// frame).
pub struct EncodedFrame {
    pub dcs: String,
    /// Height in pixels of the composed image. Used to compute how
    /// many terminal rows the sixel occupies (for blank-rect tracking).
    pub pixel_height: u32,
}

pub struct ComposedEncodedCache {
    inner: Mutex<Inner>,
    inflight: Mutex<HashMap<EncodedKey, Arc<InflightSlot>>>,
}

struct Inner {
    cache: LruCache<EncodedKey, Arc<EncodedFrame>>,
    enabled: bool,
    capacity: usize,
}

struct InflightSlot {
    result: Mutex<Option<Result<Arc<EncodedFrame>, String>>>,
    cv: Condvar,
}

impl InflightSlot {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            cv: Condvar::new(),
        }
    }
}

impl ComposedEncodedCache {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self {
            inner: Mutex::new(Inner {
                cache: LruCache::new(cap),
                enabled: true,
                capacity: capacity.max(1),
            }),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &EncodedKey) -> Option<Arc<EncodedFrame>> {
        let mut g = self.inner.lock();
        if !g.enabled {
            return None;
        }
        g.cache.get(key).cloned()
    }

    pub fn insert(&self, key: EncodedKey, value: Arc<EncodedFrame>) {
        let mut g = self.inner.lock();
        if !g.enabled {
            return;
        }
        g.cache.put(key, value);
    }

    pub fn set_enabled(&self, enabled: bool) {
        let mut g = self.inner.lock();
        g.enabled = enabled;
        if !enabled {
            g.cache.clear();
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.lock().enabled
    }

    pub fn resize(&self, capacity: usize) {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        let mut g = self.inner.lock();
        g.cache.resize(cap);
        g.capacity = capacity.max(1);
    }

    pub fn clear(&self) {
        self.inner.lock().cache.clear();
    }

    pub fn stats(&self) -> (usize, usize) {
        let g = self.inner.lock();
        (g.cache.len(), g.capacity)
    }

    /// Cache-lookup-then-produce with single-flight. If two callers
    /// miss on the same key simultaneously, only the first runs
    /// `producer` (compose + encode, ~tens to hundreds of ms); the
    /// rest block until it finishes, then share its result. Returns
    /// the frame plus `(compose_dur, encode_dur)` — both zero for
    /// hits and followers.
    pub fn get_or_encode<F>(
        &self,
        key: EncodedKey,
        producer: F,
    ) -> Result<(Arc<EncodedFrame>, Duration, Duration)>
    where
        F: FnOnce() -> Result<(EncodedFrame, Duration, Duration)>,
    {
        if let Some(hit) = self.get(&key) {
            return Ok((hit, Duration::ZERO, Duration::ZERO));
        }

        let (leader, slot) = {
            let mut g = self.inflight.lock();
            if let Some(existing) = g.get(&key) {
                (false, existing.clone())
            } else {
                let slot = Arc::new(InflightSlot::new());
                g.insert(key, slot.clone());
                (true, slot)
            }
        };

        if !leader {
            let mut g = slot.result.lock();
            while g.is_none() {
                slot.cv.wait(&mut g);
            }
            return match g.as_ref().unwrap() {
                Ok(a) => Ok((a.clone(), Duration::ZERO, Duration::ZERO)),
                Err(e) => Err(anyhow!("shared encode failed: {e}")),
            };
        }

        let produced = producer();
        let (reply_for_slot, ret): (
            Result<Arc<EncodedFrame>, String>,
            Result<(Arc<EncodedFrame>, Duration, Duration)>,
        ) = match produced {
            Ok((frame, compose, encode)) => {
                let arc = Arc::new(frame);
                self.insert(key, arc.clone());
                (Ok(arc.clone()), Ok((arc, compose, encode)))
            }
            Err(e) => {
                let msg = format!("{e:#}");
                (Err(msg), Err(e))
            }
        };
        {
            let mut g = slot.result.lock();
            *g = Some(reply_for_slot);
        }
        slot.cv.notify_all();
        self.inflight.lock().remove(&key);
        ret
    }
}

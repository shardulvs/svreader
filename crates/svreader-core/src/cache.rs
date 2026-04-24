use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use image::RgbaImage;
use parking_lot::{Condvar, Mutex};

use crate::buffer::BufferId;
use crate::viewport::Rotation;

/// Stable key for the rasterised page bitmap. Night mode, quality,
/// and scroll offsets are intentionally NOT in the key — they're all
/// applied at compose time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub buffer: BufferId,
    pub page_idx: usize,
    /// Display scale quantised to 1/256 of a unit. Catches legitimate
    /// resolution changes without blowing up on float noise.
    pub display_scale_q: u32,
    pub raster_scale_q: u32,
    pub rotation: Rotation,
}

impl CacheKey {
    pub fn new(
        buffer: BufferId,
        page_idx: usize,
        display_scale: f32,
        raster_scale: f32,
        rotation: Rotation,
    ) -> Self {
        Self {
            buffer,
            page_idx,
            display_scale_q: (display_scale * 256.0).round().max(0.0) as u32,
            raster_scale_q: (raster_scale * 256.0).round().max(0.0) as u32,
            rotation,
        }
    }
}

pub struct CachedPage {
    pub page_idx: usize,
    pub rotation: Rotation,
    pub display_scale: f32,
    pub image: RgbaImage,
}

/// Page bitmap cache with **distance-based eviction**: when full, the
/// entry furthest (by page index) from the current focus page is
/// evicted first. Reading PDFs is overwhelmingly sequential, so
/// "furthest from current" is a much better proxy for "least useful"
/// than LRU's "least recently touched". Without this, a brief detour
/// to a far page can evict the neighbourhood we're about to return
/// to.
///
/// Also single-flights concurrent renders: two callers that miss on
/// the same key share one `producer` invocation. That's what keeps
/// the prefetch worker and the paint thread from rendering the same
/// page twice in parallel.
pub struct RenderCache {
    inner: Mutex<Inner>,
    inflight: Mutex<HashMap<CacheKey, Arc<InflightSlot>>>,
}

struct Inner {
    cache: HashMap<CacheKey, Arc<CachedPage>>,
    /// Page the user is currently reading. Eviction distance is
    /// computed against this. `None` means "no preference" — any
    /// entry is equally evictable.
    focus: Option<(BufferId, usize)>,
    enabled: bool,
    capacity: usize,
}

struct InflightSlot {
    result: Mutex<Option<Result<Arc<CachedPage>, String>>>,
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

impl RenderCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                cache: HashMap::new(),
                focus: None,
                enabled: true,
                capacity: capacity.max(1),
            }),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<Arc<CachedPage>> {
        let g = self.inner.lock();
        if !g.enabled {
            return None;
        }
        g.cache.get(key).cloned()
    }

    pub fn contains(&self, key: &CacheKey) -> bool {
        let g = self.inner.lock();
        g.enabled && g.cache.contains_key(key)
    }

    pub fn insert(&self, key: CacheKey, value: Arc<CachedPage>) {
        let mut g = self.inner.lock();
        if !g.enabled {
            return;
        }
        g.cache.insert(key, value);
        evict_to_capacity(&mut g);
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
        let mut g = self.inner.lock();
        g.capacity = capacity.max(1);
        evict_to_capacity(&mut g);
    }

    pub fn clear(&self) {
        self.inner.lock().cache.clear();
    }

    pub fn stats(&self) -> (usize, usize) {
        let g = self.inner.lock();
        (g.cache.len(), g.capacity)
    }

    /// Declare the page the user is reading right now. Subsequent
    /// evictions preserve entries near `page_idx` for `buffer` and
    /// evict pages further away (or pages of other buffers) first.
    pub fn set_focus(&self, buffer: BufferId, page_idx: usize) {
        let mut g = self.inner.lock();
        g.focus = Some((buffer, page_idx));
    }

    /// Find any cached entry for `(buffer, page_idx, rotation)`,
    /// regardless of display/raster scale. The ECache filler uses
    /// this: given a page that's present in the RCache at whatever
    /// scale the paint thread stored it, the filler composes +
    /// encodes from that same bitmap so the resulting ECache entry
    /// matches what a future paint would produce.
    pub fn find_matching(
        &self,
        buffer: BufferId,
        page_idx: usize,
        rotation: Rotation,
    ) -> Option<(CacheKey, Arc<CachedPage>)> {
        let g = self.inner.lock();
        if !g.enabled {
            return None;
        }
        g.cache
            .iter()
            .find(|(k, _)| {
                k.buffer == buffer && k.page_idx == page_idx && k.rotation == rotation
            })
            .map(|(k, v)| (*k, v.clone()))
    }

    /// Return the cached page if present; otherwise call `producer`
    /// under a single-flight lock so that concurrent callers for the
    /// same key share one render. `producer` returns the produced
    /// `CachedPage` together with the time spent rendering; on a hit
    /// or as a follower the returned duration is zero. `producer`
    /// owns whatever document handle it needs (the mupdf handle is
    /// `!Send`, so paint and prefetch threads each bring their own).
    pub fn get_or_render<F>(
        &self,
        key: CacheKey,
        producer: F,
    ) -> Result<(Arc<CachedPage>, Duration)>
    where
        F: FnOnce() -> Result<(CachedPage, Duration)>,
    {
        if let Some(hit) = self.get(&key) {
            return Ok((hit, Duration::ZERO));
        }

        let (leader, slot) = claim_or_follow(self, key);
        if !leader {
            let mut g = slot.result.lock();
            while g.is_none() {
                slot.cv.wait(&mut g);
            }
            return match g.as_ref().unwrap() {
                Ok(a) => Ok((a.clone(), Duration::ZERO)),
                Err(e) => Err(anyhow!("shared render failed: {e}")),
            };
        }

        // Leader: do the work.
        let produced = producer();
        let (reply_for_slot, ret): (
            Result<Arc<CachedPage>, String>,
            Result<(Arc<CachedPage>, Duration)>,
        ) = match produced {
            Ok((page, dur)) => {
                let arc = Arc::new(page);
                self.insert(key, arc.clone());
                (Ok(arc.clone()), Ok((arc, dur)))
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

fn claim_or_follow(cache: &RenderCache, key: CacheKey) -> (bool, Arc<InflightSlot>) {
    let mut g = cache.inflight.lock();
    if let Some(existing) = g.get(&key) {
        return (false, existing.clone());
    }
    let slot = Arc::new(InflightSlot::new());
    g.insert(key, slot.clone());
    (true, slot)
}

/// Evict the entry with the greatest distance from the focus page
/// until `cache.len() <= capacity`. Entries for a different buffer
/// than `focus` get a very large distance so they're evicted before
/// any in-buffer entry.
fn evict_to_capacity(inner: &mut Inner) {
    while inner.cache.len() > inner.capacity {
        let focus = inner.focus;
        let victim = inner
            .cache
            .keys()
            .max_by_key(|k| distance_to_focus(k, focus))
            .copied();
        match victim {
            Some(v) => {
                inner.cache.remove(&v);
            }
            None => break,
        }
    }
}

fn distance_to_focus(key: &CacheKey, focus: Option<(BufferId, usize)>) -> u64 {
    match focus {
        Some((buf, page)) => {
            if buf != key.buffer {
                // Different buffer: evict first, but keep below u64::MAX
                // so we don't trip any future tie-breakers using the
                // extreme value.
                u64::MAX / 2
            } else {
                (key.page_idx as i64 - page as i64).unsigned_abs()
            }
        }
        None => 0,
    }
}

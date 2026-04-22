use std::num::NonZeroUsize;
use std::sync::Arc;

use image::RgbaImage;
use lru::LruCache;
use parking_lot::Mutex;

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

pub struct PageCache {
    inner: Mutex<Inner>,
}

struct Inner {
    cache: LruCache<CacheKey, Arc<CachedPage>>,
    enabled: bool,
    capacity: usize,
}

impl PageCache {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self {
            inner: Mutex::new(Inner {
                cache: LruCache::new(cap),
                enabled: true,
                capacity: capacity.max(1),
            }),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<Arc<CachedPage>> {
        let mut g = self.inner.lock();
        if !g.enabled {
            return None;
        }
        g.cache.get(key).cloned()
    }

    pub fn contains(&self, key: &CacheKey) -> bool {
        let g = self.inner.lock();
        g.enabled && g.cache.contains(key)
    }

    pub fn insert(&self, key: CacheKey, value: Arc<CachedPage>) {
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
}

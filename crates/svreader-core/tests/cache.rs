use std::sync::Arc;

use image::RgbaImage;
use svreader_core::buffer::BufferId;
use svreader_core::cache::{CacheKey, CachedPage, PageCache};
use svreader_core::Rotation;

fn key(i: usize) -> CacheKey {
    CacheKey::new(BufferId(1), i, 1.0, 1.0, Rotation::R0)
}

fn page(i: usize) -> Arc<CachedPage> {
    Arc::new(CachedPage {
        page_idx: i,
        rotation: Rotation::R0,
        display_scale: 1.0,
        image: RgbaImage::new(2, 2),
    })
}

#[test]
fn insert_and_get() {
    let c = PageCache::new(3);
    c.insert(key(1), page(1));
    assert!(c.contains(&key(1)));
    let got = c.get(&key(1)).unwrap();
    assert_eq!(got.page_idx, 1);
}

#[test]
fn lru_evicts_oldest() {
    let c = PageCache::new(2);
    c.insert(key(1), page(1));
    c.insert(key(2), page(2));
    c.insert(key(3), page(3));
    assert!(!c.contains(&key(1)));
    assert!(c.contains(&key(2)));
    assert!(c.contains(&key(3)));
}

#[test]
fn disable_clears() {
    let c = PageCache::new(4);
    c.insert(key(1), page(1));
    assert!(c.contains(&key(1)));
    c.set_enabled(false);
    assert!(!c.contains(&key(1)));
    assert_eq!(c.stats().0, 0);
    // New insertions while disabled are ignored.
    c.insert(key(2), page(2));
    assert!(!c.contains(&key(2)));
    c.set_enabled(true);
    c.insert(key(3), page(3));
    assert!(c.contains(&key(3)));
}

#[test]
fn resize_evicts_if_needed() {
    let c = PageCache::new(5);
    for i in 0..5 {
        c.insert(key(i), page(i));
    }
    assert_eq!(c.stats().0, 5);
    c.resize(2);
    let (used, cap) = c.stats();
    assert_eq!(cap, 2);
    assert!(used <= 2);
}

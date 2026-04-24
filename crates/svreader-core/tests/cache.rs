use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use image::RgbaImage;
use svreader_core::buffer::BufferId;
use svreader_core::cache::{CacheKey, CachedPage, RenderCache};
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
    let c = RenderCache::new(3);
    c.insert(key(1), page(1));
    assert!(c.contains(&key(1)));
    let got = c.get(&key(1)).unwrap();
    assert_eq!(got.page_idx, 1);
}

#[test]
fn distance_evicts_farthest_from_focus() {
    // With focus on page 10, a later insertion that pushes the cache
    // past capacity should evict whichever entry is furthest from
    // page 10 — not the oldest. This is the "farthest from current
    // page" policy tailored for sequential reading.
    let c = RenderCache::new(3);
    c.set_focus(BufferId(1), 10);
    c.insert(key(9), page(9));
    c.insert(key(11), page(11));
    c.insert(key(20), page(20)); // far from 10
    c.insert(key(12), page(12)); // over capacity — 20 should go
    assert!(c.contains(&key(9)));
    assert!(c.contains(&key(11)));
    assert!(c.contains(&key(12)));
    assert!(!c.contains(&key(20)));
}

#[test]
fn entries_for_other_buffers_evict_first() {
    let c = RenderCache::new(2);
    c.set_focus(BufferId(1), 5);
    // Seed with an entry from a different buffer.
    let other = CacheKey::new(BufferId(2), 100, 1.0, 1.0, Rotation::R0);
    c.insert(other, page(100));
    c.insert(key(5), page(5));
    c.insert(key(6), page(6)); // should evict the other-buffer entry first
    assert!(!c.contains(&other));
    assert!(c.contains(&key(5)));
    assert!(c.contains(&key(6)));
}

#[test]
fn resize_down_evicts_by_distance() {
    let c = RenderCache::new(5);
    c.set_focus(BufferId(1), 0);
    for i in 0..5 {
        c.insert(key(i), page(i));
    }
    c.resize(2);
    // Nearest two to page 0 survive: pages 0 and 1.
    let (used, cap) = c.stats();
    assert_eq!(cap, 2);
    assert_eq!(used, 2);
    assert!(c.contains(&key(0)));
    assert!(c.contains(&key(1)));
    assert!(!c.contains(&key(4)));
}

#[test]
fn find_matching_scans_ignoring_scale() {
    let c = RenderCache::new(4);
    let k_a = CacheKey::new(BufferId(1), 7, 1.0, 1.0, Rotation::R0);
    let k_b = CacheKey::new(BufferId(1), 7, 2.0, 2.0, Rotation::R0);
    c.insert(k_a, page(7));
    // Lookup by (buffer, page, rotation) should return whatever's
    // cached, regardless of the scales passed at insert time.
    let found = c.find_matching(BufferId(1), 7, Rotation::R0);
    assert!(found.is_some());
    let (k, _) = found.unwrap();
    assert_eq!(k, k_a);
    // Different page: no match.
    assert!(c.find_matching(BufferId(1), 8, Rotation::R0).is_none());
    let _ = k_b;
}

#[test]
fn disable_clears() {
    let c = RenderCache::new(4);
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
fn single_flight_dedupes_concurrent_renders() {
    // Two threads race on the same key. The producer is slow (sleep)
    // so that the second caller is guaranteed to find an in-flight
    // slot rather than a populated cache. We assert the producer ran
    // exactly once — this is the guarantee the prefetch worker
    // relies on to not duplicate the paint thread's work.
    let cache = Arc::new(RenderCache::new(4));
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    let threads: Vec<_> = (0..2)
        .map(|i| {
            let cache = cache.clone();
            let calls = calls.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                let (got, dur) = cache
                    .get_or_render(key(7), || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(50));
                        let cp = CachedPage {
                            page_idx: 7,
                            rotation: Rotation::R0,
                            display_scale: 1.0,
                            image: RgbaImage::new(2, 2),
                        };
                        Ok((cp, Duration::from_millis(50)))
                    })
                    .unwrap();
                assert_eq!(got.page_idx, 7);
                (i, dur)
            })
        })
        .collect();

    let mut leader_durations = 0;
    for t in threads {
        let (_, dur) = t.join().unwrap();
        if dur > Duration::ZERO {
            leader_durations += 1;
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "producer ran more than once");
    assert_eq!(leader_durations, 1, "exactly one call should report non-zero render duration");
    assert!(cache.contains(&key(7)));
}

#[test]
fn get_or_render_caches_result_for_later_callers() {
    let cache = RenderCache::new(4);
    let calls = AtomicUsize::new(0);
    for _ in 0..3 {
        let (p, _) = cache
            .get_or_render(key(3), || {
                calls.fetch_add(1, Ordering::SeqCst);
                let cp = CachedPage {
                    page_idx: 3,
                    rotation: Rotation::R0,
                    display_scale: 1.0,
                    image: RgbaImage::new(2, 2),
                };
                Ok((cp, Duration::ZERO))
            })
            .unwrap();
        assert_eq!(p.page_idx, 3);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

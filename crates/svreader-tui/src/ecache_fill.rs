//! Background encoder that keeps the ECache populated around the
//! user's current viewport.
//!
//! After every paint the main thread sends a refill request: *the
//! user is at viewport V in buffer B; please encode the frames
//! they're most likely to ask for next and stuff them into the
//! ECache so j/k are instantaneous.*
//!
//! The filler predicts "likely next viewports" by **running the
//! actual Navigator** forward and backward from the request's
//! viewport. Forward walks: `NextScreen`, `NextScreen`, … (the
//! action bound to `j`). Backward walks: `PrevScreen`, `PrevScreen`,
//! … (the action bound to `k`). Using the real Navigator guarantees
//! that the viewports we pre-encode are byte-for-byte what the
//! paint thread will later produce — no second reimplementation of
//! scroll-overlap or page-boundary anchoring to drift out of sync
//! with `navigator.rs`.
//!
//! Two strict rules are preserved:
//!
//! 1. **Never render.** If a target page is not already in the
//!    RenderCache we skip it. The RCache prefetcher is responsible
//!    for getting pages into the RCache; the ECache filler only
//!    rides on top of what's already there.
//!
//! 2. **Cancel stale work promptly.** Between each encode we peek
//!    the channel; if a newer request is waiting we abandon the
//!    current walk.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::Result;
use svreader_core::{
    Action, BufferId, CacheKey, PageInfo, PageMetrics, RenderCache, Renderer, Viewport,
};

use crate::encoded_cache::{ComposedEncodedCache, EncodedFrame, EncodedKey};
use crate::sixel_write::{encode_rgba_to_dcs, ColorMode};

#[derive(Debug, Clone)]
pub struct RefillRequest {
    pub buffer: BufferId,
    /// Snapshot of the focused window's viewport at the moment the
    /// request was issued. The filler uses this as the starting
    /// state for Navigator walks.
    pub viewport: Viewport,
    pub color_mode: ColorMode,
    /// Page-size snapshot used to run Navigator without holding the
    /// mupdf document.
    pub page_info: PageInfo,
    /// How many steps forward and backward to walk. The filler
    /// stops early if Navigator produces a no-op (end of document)
    /// or a cancel arrives.
    pub radius: usize,
    /// Monotonic token stamped by `EncCacheFiller::request`.
    pub generation: u64,
}

impl RefillRequest {
    pub fn new(
        buffer: BufferId,
        viewport: Viewport,
        color_mode: ColorMode,
        page_info: PageInfo,
        radius: usize,
    ) -> Self {
        Self {
            buffer,
            viewport,
            color_mode,
            page_info,
            radius,
            generation: 0,
        }
    }
}

pub struct EncCacheFiller {
    tx: Sender<Msg>,
    generation: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

enum Msg {
    Refill(RefillRequest),
    Shutdown,
}

impl EncCacheFiller {
    pub fn spawn(
        rcache: Arc<RenderCache>,
        ecache: Arc<ComposedEncodedCache>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Msg>();
        let generation = Arc::new(AtomicU64::new(0));
        let gen_for_worker = generation.clone();
        let handle = thread::Builder::new()
            .name("svreader-ecache-fill".into())
            .spawn(move || {
                worker_loop(rcache, ecache, rx, gen_for_worker);
            })?;
        Ok(Self {
            tx,
            generation,
            handle: Some(handle),
        })
    }

    /// Bump the generation and queue a new refill request.
    pub fn request(&self, mut req: RefillRequest) -> u64 {
        let g = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        req.generation = g;
        let _ = self.tx.send(Msg::Refill(req));
        g
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for EncCacheFiller {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(
    rcache: Arc<RenderCache>,
    ecache: Arc<ComposedEncodedCache>,
    rx: Receiver<Msg>,
    generation: Arc<AtomicU64>,
) {
    while let Ok(msg) = rx.recv() {
        let mut current = match msg {
            Msg::Shutdown => return,
            Msg::Refill(r) => r,
        };
        // Drain any requests piled up while blocked; only the most
        // recent one matters.
        loop {
            match rx.try_recv() {
                Ok(Msg::Refill(r)) => current = r,
                Ok(Msg::Shutdown) => return,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        process_request(&rcache, &ecache, &generation, current);
    }
}

/// Walk Navigator forward and backward from the requested viewport,
/// encoding each predicted viewport if its page is already in the
/// RCache.
fn process_request(
    rcache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
    generation: &Arc<AtomicU64>,
    req: RefillRequest,
) {
    if req.page_info.page_count() == 0 {
        return;
    }
    walk(rcache, ecache, generation, &req, Action::NextScreen);
    walk(rcache, ecache, generation, &req, Action::PrevScreen);
}

/// Walk `req.radius` steps in one direction (forward or backward)
/// by repeatedly applying `action` with Navigator. Encodes each
/// resulting viewport that has an already-cached page.
fn walk(
    rcache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
    generation: &Arc<AtomicU64>,
    req: &RefillRequest,
    action: Action,
) {
    let mut vp = req.viewport.clone();
    let mut last_signature: Option<(usize, i32, i32)> = Some(signature(&vp));
    for _ in 0..req.radius {
        if !still_current(generation, req) {
            return;
        }
        // Run the real Navigator against our page-info snapshot.
        // This is the whole point of the refactor: the target
        // viewport is whatever the paint thread would produce if
        // the user actually pressed this key, because we're calling
        // the same function.
        if svreader_core::Navigator::apply(&req.page_info, &mut vp, action.clone()).is_err() {
            return;
        }
        let sig = signature(&vp);
        if last_signature == Some(sig) {
            // Navigator was a no-op (end of document, page fits and
            // no next page, etc.). Nothing more to pre-encode in
            // this direction.
            return;
        }
        last_signature = Some(sig);
        try_fill(rcache, ecache, req, &vp);
    }
}

fn signature(vp: &Viewport) -> (usize, i32, i32) {
    (vp.page_idx, vp.x_off, vp.y_off)
}

/// Encode one predicted viewport if its page is already in the
/// RCache and it isn't already in the ECache. Purely opportunistic:
/// a miss on the RCache is NOT an error, it just means "not yet."
fn try_fill(
    rcache: &Arc<RenderCache>,
    ecache: &Arc<ComposedEncodedCache>,
    req: &RefillRequest,
    vp: &Viewport,
) {
    if !ecache.enabled() {
        return;
    }
    let page_size = match req.page_info.page_size(vp.page_idx) {
        Ok(s) => s,
        Err(_) => return,
    };
    let display_scale = vp.display_scale(page_size);
    let raster_scale = vp.raster_scale(page_size);
    let rkey = CacheKey::new(
        req.buffer,
        vp.page_idx,
        display_scale,
        raster_scale,
        vp.rotation,
    );
    let Some(cached) = rcache.get(&rkey) else {
        // Not rendered yet. Prefetcher is responsible for that; the
        // filler never triggers a render itself.
        return;
    };
    let ekey = EncodedKey::from_viewport(req.buffer, vp, display_scale, raster_scale, req.color_mode);
    if ecache.get(&ekey).is_some() {
        return;
    }
    let cp = cached.clone();
    let vp_clone = vp.clone();
    let color_mode = req.color_mode;
    let res = ecache.get_or_encode(ekey, || {
        let (composed, ct) = Renderer::compose(&cp, &vp_clone);
        let (dcs, encode_dur) = encode_rgba_to_dcs(&composed, color_mode)?;
        let frame = EncodedFrame {
            dcs,
            pixel_height: composed.height(),
        };
        Ok((frame, ct.compose, encode_dur))
    });
    if let Err(e) = res {
        tracing::debug!(target: "svreader::ecache_fill", "fill failed: {e:#}");
    }
}

/// Returns true if `req` is still the request we should be working
/// on. Checked via the atomic generation counter only — we can't
/// peek the channel non-destructively, so we rely on the fact that
/// `EncCacheFiller::request` bumps the atomic *before* sending.
/// A stale request bails; `worker_loop` then picks the newer
/// message up on its next `recv`.
fn still_current(generation: &Arc<AtomicU64>, req: &RefillRequest) -> bool {
    let latest = generation.load(Ordering::SeqCst);
    req.generation >= latest
}

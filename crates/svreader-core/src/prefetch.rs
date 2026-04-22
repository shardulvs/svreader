use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::Result;

use crate::cache::{CacheKey, PageCache};
use crate::pdf::PdfDocument;
use crate::renderer::Renderer;
use crate::viewport::Viewport;

#[derive(Debug, Clone)]
pub struct PrefetchRequest {
    pub viewport: Viewport,
    pub key: CacheKey,
}

/// Background prefetch worker. The worker thread opens its own
/// `PdfDocument` from the same path because mupdf handles are not
/// `Send`.
pub struct Prefetcher {
    tx: Sender<PrefetchMsg>,
    handle: Option<JoinHandle<()>>,
}

enum PrefetchMsg {
    Prefetch(PrefetchRequest),
    Shutdown,
}

impl Prefetcher {
    pub fn spawn(main_doc: &PdfDocument, cache: Arc<PageCache>) -> Result<Self> {
        let path: PathBuf = main_doc.path().to_path_buf();
        let (tx, rx) = mpsc::channel::<PrefetchMsg>();
        let handle = thread::Builder::new()
            .name("svreader-prefetch".into())
            .spawn(move || {
                let doc = match PdfDocument::open(&path) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(
                            target: "svreader::prefetch",
                            "worker failed to open PDF: {e:#}"
                        );
                        return;
                    }
                };
                worker_loop(doc, cache, rx);
            })?;
        Ok(Self {
            tx,
            handle: Some(handle),
        })
    }

    pub fn request(&self, req: PrefetchRequest) {
        let _ = self.tx.send(PrefetchMsg::Prefetch(req));
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(PrefetchMsg::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(doc: PdfDocument, cache: Arc<PageCache>, rx: Receiver<PrefetchMsg>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            PrefetchMsg::Shutdown => break,
            PrefetchMsg::Prefetch(req) => {
                if !cache.enabled() {
                    continue;
                }
                if cache.contains(&req.key) {
                    continue;
                }
                match Renderer::render_page(&doc, &req.viewport) {
                    Ok((page, _)) => {
                        cache.insert(req.key, Arc::new(page));
                    }
                    Err(e) => {
                        tracing::warn!(target: "svreader::prefetch", "prefetch failed: {e:#}");
                    }
                }
            }
        }
    }
}

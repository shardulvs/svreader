//! Open-buffer bookkeeping.
//!
//! A `Buffer` is what's behind a `Window`. Two flavours today:
//! `PdfBuffer` for PDF documents and `ExplorerBuffer` for the
//! netrw-style directory browser. `BufferId` tags every buffer so the
//! page cache can safely hold bitmaps from multiple open PDFs without
//! collisions.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::Result;

use crate::cache::RenderCache;
use crate::docstate::DocState;
use crate::document::{Document, MatchRect, PageInfo, PageMetrics};
use crate::pdf::PdfDocument;
use crate::prefetch::Prefetcher;

/// Stable identifier for an open buffer. Drives `CacheKey` so two
/// PDFs open at once don't mix up their raster bitmaps.
///
/// Values are handed out by `BufferIdSource::next()`, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u32);

#[derive(Debug, Default)]
pub struct BufferIdSource {
    next: AtomicU32,
}

impl BufferIdSource {
    pub fn new() -> Self {
        // Start at 1 so 0 is a harmless "unset" sentinel.
        Self {
            next: AtomicU32::new(1),
        }
    }

    pub fn next(&self) -> BufferId {
        BufferId(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

/// Anything a window can display. The TUI paints each variant
/// differently (sixel for PDFs, text rows for explorer) but both
/// share `BufferId` and live in the same pool.
pub enum Buffer {
    Pdf(PdfBuffer),
    Explorer(ExplorerBuffer),
}

impl Buffer {
    pub fn id(&self) -> BufferId {
        match self {
            Buffer::Pdf(p) => p.id,
            Buffer::Explorer(e) => e.id,
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Buffer::Pdf(p) => p.display_name(),
            Buffer::Explorer(e) => e.display_name(),
        }
    }

    pub fn is_explorer(&self) -> bool {
        matches!(self, Buffer::Explorer(_))
    }

    pub fn as_pdf(&self) -> Option<&PdfBuffer> {
        match self {
            Buffer::Pdf(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_pdf_mut(&mut self) -> Option<&mut PdfBuffer> {
        match self {
            Buffer::Pdf(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_explorer(&self) -> Option<&ExplorerBuffer> {
        match self {
            Buffer::Explorer(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_explorer_mut(&mut self) -> Option<&mut ExplorerBuffer> {
        match self {
            Buffer::Explorer(e) => Some(e),
            _ => None,
        }
    }
}

/// One step in the per-buffer jump history. Captures enough of the
/// viewport to put the user back exactly where they were before the
/// jump (page + scroll), so `<C-o>` after a TOC-jump returns them to
/// the body text they were reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JumpEntry {
    pub page_idx: usize,
    pub x_off: i32,
    pub y_off: i32,
}

/// In-memory search state for a buffer. Owns the active query, every
/// hit across the document, and which hit `n` / `N` should land on
/// next. Cleared by Esc and by re-entering search mode.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// Current query. Empty when no search is active.
    pub query: String,
    /// All hits across every page, sorted by `(page_idx, top, left)`.
    pub matches: Vec<MatchRect>,
    /// Index into `matches` for the currently-focused hit. None when
    /// the result list is empty or no match has been visited yet.
    pub current: Option<usize>,
    /// Direction the user last searched in. Drives `n` / `N`.
    pub forward: bool,
    /// Bumped on every state change (new query, cleared, current
    /// moved). The TUI compares this against the version it last
    /// composed for to decide whether the encoded-frame cache needs a
    /// flush before painting the highlights.
    pub version: u64,
}

impl SearchState {
    pub fn is_active(&self) -> bool {
        !self.query.is_empty() && !self.matches.is_empty()
    }

    pub fn clear(&mut self) {
        if self.query.is_empty() && self.matches.is_empty() && self.current.is_none() {
            return;
        }
        self.query.clear();
        self.matches.clear();
        self.current = None;
        self.version = self.version.wrapping_add(1);
    }
}

/// A PDF the user has opened. One instance per distinct path; two
/// windows can hold references to the same buffer for vim-style shared
/// buffers via the workspace's reference-counted pool.
pub struct PdfBuffer {
    pub id: BufferId,
    pub path: PathBuf,
    pub pdf: PdfDocument,
    pub state: DocState,
    /// `Send` snapshot of page geometry. Cloned cheaply (it's an
    /// `Arc` inside) and handed to background workers so they can
    /// run Navigator without holding the mupdf document itself.
    pub page_info: PageInfo,
    /// Per-buffer prefetcher: owns its own mupdf handle (not `Send`)
    /// and dies with the buffer.
    pub prefetcher: Prefetcher,
    /// Vim-style jump list: `<C-o>` walks backward, `:forward` walks
    /// forward. In-memory only — matches vim's default behaviour
    /// where the jump list isn't persisted.
    pub back_stack: Vec<JumpEntry>,
    pub forward_stack: Vec<JumpEntry>,
    /// Lazy cache of internal links per page. Filled on demand the
    /// first time the page is hit-tested. Empty `Vec` means "no
    /// links on this page" (still cached so we don't re-query).
    pub link_cache: std::collections::HashMap<usize, Vec<crate::document::PageLink>>,
    /// Active search state. Empty until the user runs `/`.
    pub search: SearchState,
}

impl PdfBuffer {
    pub fn open(
        id: BufferId,
        path: impl AsRef<Path>,
        cache: Arc<RenderCache>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let pdf = PdfDocument::open(&path)?;
        let state = DocState::load(&path).unwrap_or_default();
        let page_info = PageInfo::from_metrics(&pdf)?;
        let prefetcher = Prefetcher::spawn(&pdf, cache)?;
        Ok(Self {
            id,
            path,
            pdf,
            state,
            page_info,
            prefetcher,
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            link_cache: std::collections::HashMap::new(),
            search: SearchState::default(),
        })
    }

    /// Run `query` against every page; replace `self.search` with the
    /// result. Picks an initial `current` match by scanning forward
    /// from `from_page` (when `forward`) or backward (otherwise),
    /// matching vim's `/` (forward) and `?` (reverse) semantics.
    pub fn run_search(&mut self, query: &str, from_page: usize, forward: bool) {
        let mut state = SearchState {
            query: query.to_string(),
            matches: Vec::new(),
            current: None,
            forward,
            version: self.search.version.wrapping_add(1),
        };
        if query.is_empty() {
            self.search = state;
            return;
        }
        let n = self.pdf.page_count();
        for page in 0..n {
            match self.pdf.page_search(page, query) {
                Ok(mut hits) => state.matches.append(&mut hits),
                Err(e) => {
                    tracing::warn!("page_search({page}, {query:?}): {e:#}");
                }
            }
        }
        state.current = pick_initial_match(&state.matches, from_page, forward);
        self.search = state;
    }

    /// Step `n` (or back one with `N`) through the existing search
    /// results. Returns the new current `MatchRect` if the search has
    /// matches, otherwise `None`. `step_forward = true` is `n`,
    /// `false` is `N` — it's not the *original* search direction, just
    /// "which way do we want to go now."
    pub fn step_search(&mut self, step_forward: bool) -> Option<MatchRect> {
        let total = self.search.matches.len();
        if total == 0 {
            return None;
        }
        let next = match self.search.current {
            None => {
                if step_forward {
                    0
                } else {
                    total - 1
                }
            }
            Some(i) => {
                if step_forward {
                    (i + 1) % total
                } else {
                    (i + total - 1) % total
                }
            }
        };
        self.search.current = Some(next);
        self.search.version = self.search.version.wrapping_add(1);
        self.search.matches.get(next).copied()
    }

    /// Forget the active search (clears highlights). Returns true if
    /// something was actually cleared, so callers can decide whether
    /// they need a repaint.
    pub fn clear_search(&mut self) -> bool {
        if !self.search.is_active() && self.search.query.is_empty() {
            return false;
        }
        self.search.clear();
        true
    }

    /// Look up cached links for `page_idx`, querying mupdf and caching
    /// the result on first access. Returns an empty slice on render
    /// errors.
    pub fn links_for(&mut self, page_idx: usize) -> &[crate::document::PageLink] {
        if !self.link_cache.contains_key(&page_idx) {
            let links = match self.pdf.page_links(page_idx) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("page_links({page_idx}): {e:#}");
                    Vec::new()
                }
            };
            self.link_cache.insert(page_idx, links);
        }
        self.link_cache.get(&page_idx).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Filename for display (falls back to "document").
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into())
    }
}

/// A filesystem browser sitting in a window. Lists the directories
/// and supported documents at `cwd`; Enter descends into directories
/// or promotes a PDF entry into the focused window as a `PdfBuffer`.
pub struct ExplorerBuffer {
    pub id: BufferId,
    pub cwd: PathBuf,
    pub entries: Vec<ExplorerEntry>,
    /// Index into `entries`. Clamped on refresh.
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerEntry {
    pub name: String,
    pub kind: ExplorerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorerKind {
    /// Parent-directory pseudo-entry (`..`). Always rendered first
    /// when the cwd has a parent.
    ParentDir,
    Dir,
    Pdf,
}

impl ExplorerEntry {
    pub fn is_dir_like(&self) -> bool {
        matches!(self.kind, ExplorerKind::Dir | ExplorerKind::ParentDir)
    }
}

/// Pick the index of the first match the cursor should land on. Matches
/// vim's `/` semantics: forward search lands on the first hit at or
/// after `from_page`, wrapping to the start if none exists; reverse
/// search lands on the last hit at or before `from_page`, wrapping
/// to the end.
fn pick_initial_match(
    matches: &[MatchRect],
    from_page: usize,
    forward: bool,
) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    if forward {
        for (i, m) in matches.iter().enumerate() {
            if m.page_idx >= from_page {
                return Some(i);
            }
        }
        Some(0)
    } else {
        for (i, m) in matches.iter().enumerate().rev() {
            if m.page_idx <= from_page {
                return Some(i);
            }
        }
        Some(matches.len() - 1)
    }
}

/// Extensions the reader knows how to open. Kept in sync with
/// `svreader-tui`'s palette filter. Extend this list alongside new
/// `Document` backends (EPUB, DjVu, CBZ).
pub const EXPLORER_SUPPORTED_EXTS: &[&str] = &["pdf"];

fn is_supported_doc(name: &str) -> bool {
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        return false;
    }
    let lower = ext.to_ascii_lowercase();
    EXPLORER_SUPPORTED_EXTS.iter().any(|e| *e == lower)
}

impl ExplorerBuffer {
    pub fn open(id: BufferId, cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| cwd.as_ref().to_path_buf());
        let mut buf = Self {
            id,
            cwd,
            entries: Vec::new(),
            selected: 0,
        };
        buf.refresh()?;
        Ok(buf)
    }

    /// Re-scan `cwd`. Preserves the selected entry name across
    /// refreshes so `..` → descend → refresh lands you on the same
    /// file the cursor was on before.
    pub fn refresh(&mut self) -> Result<()> {
        let prev_name = self.entries.get(self.selected).map(|e| e.name.clone());
        let mut entries: Vec<ExplorerEntry> = Vec::new();

        if self.cwd.parent().is_some() {
            entries.push(ExplorerEntry {
                name: "..".into(),
                kind: ExplorerKind::ParentDir,
            });
        }

        let read = std::fs::read_dir(&self.cwd);
        let mut dirs: Vec<ExplorerEntry> = Vec::new();
        let mut files: Vec<ExplorerEntry> = Vec::new();
        if let Ok(rd) = read {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    if name.ends_with(".sdr") {
                        // koreader-style metadata sidecar — hide.
                        continue;
                    }
                    dirs.push(ExplorerEntry {
                        name,
                        kind: ExplorerKind::Dir,
                    });
                } else if is_supported_doc(&name) {
                    files.push(ExplorerEntry {
                        name,
                        kind: ExplorerKind::Pdf,
                    });
                }
            }
        }
        dirs.sort_by(|a, b| a.name.cmp(&b.name));
        files.sort_by(|a, b| a.name.cmp(&b.name));
        entries.extend(dirs);
        entries.extend(files);

        self.entries = entries;
        self.selected = match prev_name {
            Some(n) => self
                .entries
                .iter()
                .position(|e| e.name == n)
                .unwrap_or(0),
            None => 0,
        };
        self.clamp_selection();
        Ok(())
    }

    pub fn clamp_selection(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.entries.len() {
            self.selected = self.entries.len() - 1;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as isize;
        let cur = self.selected as isize;
        let mut next = cur + delta;
        if next < 0 {
            next = 0;
        }
        if next >= n {
            next = n - 1;
        }
        self.selected = next as usize;
    }

    pub fn goto_first(&mut self) {
        self.selected = 0;
    }

    pub fn goto_last(&mut self) {
        if !self.entries.is_empty() {
            self.selected = self.entries.len() - 1;
        }
    }

    pub fn selected_entry(&self) -> Option<&ExplorerEntry> {
        self.entries.get(self.selected)
    }

    /// Resolve the selected entry to a path on disk.
    pub fn selected_path(&self) -> Option<PathBuf> {
        let e = self.selected_entry()?;
        match e.kind {
            ExplorerKind::ParentDir => self.cwd.parent().map(|p| p.to_path_buf()),
            ExplorerKind::Dir | ExplorerKind::Pdf => Some(self.cwd.join(&e.name)),
        }
    }

    /// Change `cwd` and rescan.
    pub fn set_cwd(&mut self, new: PathBuf) -> Result<()> {
        let canon = new.canonicalize().unwrap_or(new);
        self.cwd = canon;
        self.entries.clear();
        self.selected = 0;
        self.refresh()
    }

    /// Move to parent dir, keeping the previously-current dir selected
    /// (so the user can re-enter with `l`).
    pub fn parent(&mut self) -> Result<()> {
        let Some(parent) = self.cwd.parent().map(|p| p.to_path_buf()) else {
            return Ok(());
        };
        let child_name = self
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned());
        self.cwd = parent.canonicalize().unwrap_or(parent);
        self.refresh()?;
        if let Some(name) = child_name {
            if let Some(idx) = self.entries.iter().position(|e| e.name == name) {
                self.selected = idx;
            }
        }
        Ok(())
    }

    /// Short title used by the tab bar and window status.
    pub fn display_name(&self) -> String {
        let base = self
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned());
        format!("[{}]", base)
    }
}

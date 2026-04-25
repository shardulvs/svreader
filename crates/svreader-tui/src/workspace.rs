//! Top-level state for the reader: buffer pool + tabs + dispatch.
//!
//! This is what owns "what's on screen". The event loop in
//! `render_loop.rs` is thin — it pumps keys / commands at the
//! workspace, and then paints whatever `Workspace::layout` says.
//!
//! M1.5a: PDF buffers only. M1.5b will add an `ExplorerBuffer`
//! variant behind a `Buffer` enum.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use svreader_core::keys::WindowOp;
use svreader_core::{
    Action, Buffer, BufferId, BufferIdSource, ExplorerBuffer, JumpEntry, Navigator, PageLink,
    PageMetrics, ParsedCommand, PdfBuffer, RenderCache, SplitDirection, Viewport,
};

use crate::ecache_fill::EncCacheFiller;
use crate::encoded_cache::ComposedEncodedCache;
use crate::window::{
    Axis, CellRect, CloseOutcome, Direction, Window, WindowId, WindowIdSource, WindowTree,
};

/// One tab: an independent split tree with its own focused window.
pub struct Tab {
    pub tree: WindowTree,
    pub focused: WindowId,
}

impl Tab {
    fn new(tree: WindowTree) -> Self {
        let focused = tree.first_id();
        Self { tree, focused }
    }
}

/// All open buffers, PDF or explorer. Reference-counted so
/// shared-buffer splits (`:vsplit` with no argument) work cleanly: the
/// buffer's prefetcher dies when no window is looking at it any more.
///
/// PDFs dedupe by canonical path so reopening the same file just bumps
/// refs. Explorer buffers do not dedupe — each `:Ex` spawns a fresh
/// one so two windows can browse different directories.
struct BufferPool {
    by_id: HashMap<BufferId, Slot>,
    by_path: HashMap<PathBuf, BufferId>,
}

struct Slot {
    buf: Buffer,
    refs: usize,
}

impl BufferPool {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            by_path: HashMap::new(),
        }
    }

    fn open_pdf(
        &mut self,
        id_src: &BufferIdSource,
        path: impl AsRef<Path>,
        cache: Arc<RenderCache>,
    ) -> Result<BufferId> {
        let raw_path = path.as_ref().to_path_buf();
        let key_path = raw_path
            .canonicalize()
            .unwrap_or_else(|_| raw_path.clone());
        if let Some(&id) = self.by_path.get(&key_path) {
            if let Some(slot) = self.by_id.get_mut(&id) {
                slot.refs += 1;
            }
            return Ok(id);
        }
        let id = id_src.next();
        let buf = PdfBuffer::open(id, &raw_path, cache)?;
        self.by_path.insert(key_path, id);
        self.by_id
            .insert(id, Slot { buf: Buffer::Pdf(buf), refs: 1 });
        Ok(id)
    }

    fn open_explorer(
        &mut self,
        id_src: &BufferIdSource,
        cwd: impl AsRef<Path>,
    ) -> Result<BufferId> {
        let id = id_src.next();
        let buf = ExplorerBuffer::open(id, cwd)?;
        self.by_id
            .insert(id, Slot { buf: Buffer::Explorer(buf), refs: 1 });
        Ok(id)
    }

    fn acquire(&mut self, id: BufferId) {
        if let Some(slot) = self.by_id.get_mut(&id) {
            slot.refs += 1;
        }
    }

    fn release(&mut self, id: BufferId) {
        let should_drop = if let Some(slot) = self.by_id.get_mut(&id) {
            if slot.refs > 0 {
                slot.refs -= 1;
            }
            slot.refs == 0
        } else {
            false
        };
        if should_drop {
            if let Some(slot) = self.by_id.remove(&id) {
                // Last ref gone — flush the sidecar before the doc
                // drops, otherwise viewport state set during the
                // session is lost (the buffer was about to disappear
                // anyway, so there's no later `persist_all` to save
                // it).
                if let Buffer::Pdf(pdf) = &slot.buf {
                    if let Err(e) = pdf.state.save(&pdf.path) {
                        tracing::warn!(
                            "failed to save sidecar on release for {:?}: {e:#}",
                            pdf.path
                        );
                    }
                }
                self.by_path.retain(|_, v| *v != id);
                drop(slot);
            }
        }
    }

    fn get(&self, id: BufferId) -> Option<&Buffer> {
        self.by_id.get(&id).map(|s| &s.buf)
    }

    fn get_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        self.by_id.get_mut(&id).map(|s| &mut s.buf)
    }

    fn get_pdf(&self, id: BufferId) -> Option<&PdfBuffer> {
        self.get(id).and_then(Buffer::as_pdf)
    }

    fn get_pdf_mut(&mut self, id: BufferId) -> Option<&mut PdfBuffer> {
        self.get_mut(id).and_then(Buffer::as_pdf_mut)
    }

    fn ids(&self) -> Vec<BufferId> {
        let mut ids: Vec<_> = self.by_id.keys().copied().collect();
        ids.sort_by_key(|i| i.0);
        ids
    }

    /// PDF-only ids in insertion order by `BufferId`. Used by `:b`,
    /// `:bnext`, `:bprev` — explorers shouldn't show up there.
    fn pdf_ids(&self) -> Vec<BufferId> {
        let mut ids: Vec<_> = self
            .by_id
            .iter()
            .filter(|(_, s)| matches!(s.buf, Buffer::Pdf(_)))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|i| i.0);
        ids
    }
}

/// The full terminal state.
pub struct Workspace {
    pool: BufferPool,
    tabs: Vec<Tab>,
    current_tab: usize,
    win_ids: WindowIdSource,
    buf_ids: BufferIdSource,
    pub cache: Arc<RenderCache>,
    pub ecache: Arc<ComposedEncodedCache>,
    pub ecache_filler: Arc<EncCacheFiller>,
    /// Set by `:q`/`:qa` to break the outer event loop.
    pub quit_requested: bool,
    /// Transient message bubble for the status bar.
    pub message: Option<String>,
    /// Screen rect passed to `layout()` most recently. Used by window
    /// ops that need to know geometry (focus_neighbour, resize).
    pub last_rect: CellRect,
}

impl Workspace {
    /// Construct a workspace seeded with one PDF in one window in
    /// one tab.
    pub fn with_pdf(
        cache: Arc<RenderCache>,
        ecache: Arc<ComposedEncodedCache>,
        ecache_filler: Arc<EncCacheFiller>,
        path: impl AsRef<Path>,
        viewport: Viewport,
    ) -> Result<Self> {
        let buf_ids = BufferIdSource::new();
        let mut pool = BufferPool::new();
        let id = pool.open_pdf(&buf_ids, path, cache.clone())?;
        // Apply sidecar-saved cache sizes, if any.
        if let Some(Buffer::Pdf(pdf)) = pool.get(id) {
            if let Some(n) = pdf.state.cache_size {
                cache.resize(n);
            }
            if let Some(n) = pdf.state.ecache_size {
                ecache.resize(n);
            }
        }
        Self::finish_bootstrap(cache, ecache, ecache_filler, pool, buf_ids, id, viewport)
    }

    /// Construct a workspace seeded with an explorer rooted at `cwd`.
    /// Used when `svreader` is launched with no arguments.
    pub fn with_explorer(
        cache: Arc<RenderCache>,
        ecache: Arc<ComposedEncodedCache>,
        ecache_filler: Arc<EncCacheFiller>,
        cwd: impl AsRef<Path>,
        viewport: Viewport,
    ) -> Result<Self> {
        let buf_ids = BufferIdSource::new();
        let mut pool = BufferPool::new();
        let id = pool.open_explorer(&buf_ids, cwd)?;
        Self::finish_bootstrap(cache, ecache, ecache_filler, pool, buf_ids, id, viewport)
    }

    fn finish_bootstrap(
        cache: Arc<RenderCache>,
        ecache: Arc<ComposedEncodedCache>,
        ecache_filler: Arc<EncCacheFiller>,
        pool: BufferPool,
        buf_ids: BufferIdSource,
        buffer: BufferId,
        viewport: Viewport,
    ) -> Result<Self> {
        let win_ids = WindowIdSource::new();
        let window = Window::new(win_ids.next(), buffer, viewport);
        let tree = WindowTree::leaf(window);
        let tab = Tab::new(tree);
        Ok(Self {
            pool,
            tabs: vec![tab],
            current_tab: 0,
            win_ids,
            buf_ids,
            cache,
            ecache,
            ecache_filler,
            quit_requested: false,
            message: None,
            last_rect: CellRect {
                col: 0,
                row: 0,
                cols: 0,
                rows: 0,
            },
        })
    }

    pub fn current_tab(&self) -> &Tab {
        &self.tabs[self.current_tab]
    }

    pub fn current_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.current_tab]
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn current_tab_index(&self) -> usize {
        self.current_tab
    }

    pub fn tab(&self, idx: usize) -> Option<&Tab> {
        self.tabs.get(idx)
    }

    pub fn focused_window(&self) -> &Window {
        let tab = self.current_tab();
        tab.tree
            .find(tab.focused)
            .expect("focused window must exist in current tab")
    }

    pub fn focused_window_mut(&mut self) -> &mut Window {
        let idx = self.current_tab;
        let focused = self.tabs[idx].focused;
        self.tabs[idx]
            .tree
            .find_mut(focused)
            .expect("focused window must exist in current tab")
    }

    pub fn buffer(&self, id: BufferId) -> Option<&Buffer> {
        self.pool.get(id)
    }

    pub fn buffer_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        self.pool.get_mut(id)
    }

    pub fn buffer_pdf(&self, id: BufferId) -> Option<&PdfBuffer> {
        self.pool.get_pdf(id)
    }

    pub fn buffer_pdf_mut(&mut self, id: BufferId) -> Option<&mut PdfBuffer> {
        self.pool.get_pdf_mut(id)
    }

    /// True when the focused window is currently showing an explorer
    /// (used by the render loop to route keys).
    pub fn focused_is_explorer(&self) -> bool {
        let id = self.focused_window().buffer;
        self.pool
            .get(id)
            .map(Buffer::is_explorer)
            .unwrap_or(false)
    }

    /// Current tab layout in cell coordinates.
    pub fn layout(&mut self, rect: CellRect) -> Vec<(WindowId, CellRect)> {
        self.last_rect = rect;
        let tab = self.current_tab();
        tab.tree.layout(rect)
    }

    /// Mark every window in the current tab dirty. Called on resize
    /// and when the palette/help overlay closes.
    pub fn mark_all_dirty(&mut self) {
        for w in self.tabs[self.current_tab].tree.windows_mut() {
            w.dirty = true;
        }
    }

    /// Re-apply a pixel-size change to a single window through the
    /// Navigator, so fit-width / fit-page zoom offsets re-anchor
    /// cleanly. Split into a Workspace method so the caller doesn't
    /// have to juggle disjoint borrows of `pool` and `tabs`.
    pub fn resync_window_viewport(
        &mut self,
        window: WindowId,
        img_w: u32,
        img_h: u32,
    ) -> Result<bool> {
        let tab = &mut self.tabs[self.current_tab];
        let Some(w) = tab.tree.find_mut(window) else {
            return Ok(false);
        };
        if w.viewport.screen_w == img_w && w.viewport.screen_h == img_h {
            return Ok(false);
        }
        let buf_id = w.buffer;
        let Some(slot) = self.pool.by_id.get(&buf_id) else {
            return Ok(false);
        };
        match &slot.buf {
            Buffer::Pdf(pdf) => {
                Navigator::apply(&pdf.pdf, &mut w.viewport, Action::Resize(img_w, img_h))?;
            }
            Buffer::Explorer(_) => {
                // Explorers don't care about page geometry — just keep
                // the new screen dims so a later swap to a PDF buffer
                // starts with the correct size.
                w.viewport.screen_w = img_w.max(1);
                w.viewport.screen_h = img_h.max(1);
            }
        }
        Ok(true)
    }

    /// Propagate a new screen rect to every window's viewport. Called
    /// when the terminal is resized and after a split/close changes
    /// per-window geometry.
    pub fn propagate_geometry(&mut self, cell_px_w: u16, cell_px_h: u16, rect: CellRect) {
        let layout = {
            let tab = &self.tabs[self.current_tab];
            tab.tree.layout(rect)
        };
        let layout_map: HashMap<WindowId, CellRect> = layout.into_iter().collect();
        let reserved_status: u16 = 1;
        for w in self.tabs[self.current_tab].tree.windows_mut() {
            if let Some(r) = layout_map.get(&w.id).copied() {
                // Window's image area = rect minus its own 1-row status
                // strip (the focused window's bottom row). For now
                // every window reserves one row for its title.
                let usable_rows = r.rows.saturating_sub(reserved_status).max(1);
                let px_w = (r.cols as u32) * (cell_px_w as u32);
                let px_h = (usable_rows as u32) * (cell_px_h as u32);
                let id = w.buffer;
                w.viewport.screen_w = px_w.max(1);
                w.viewport.screen_h = px_h.max(1);
                w.dirty = true;
                let _ = id;
            }
        }
    }

    // -------------------- nav dispatch --------------------

    pub fn apply_nav(&mut self, action: Action, count: usize) -> Result<()> {
        // Vim-style jump list bookkeeping: long-distance moves (`gg`,
        // `G`, `:goto N`) push the pre-jump viewport so `<C-o>` works.
        // `JumpTo` is excluded because it's only emitted by code
        // paths that manage the stack themselves (jump_to_page,
        // jump_back, jump_forward, mouse click-to-follow).
        if is_jump_action(&action) {
            self.push_back_stack();
        }

        let focused_id = self.current_tab().focused;
        let buffer_id;
        {
            let win = self
                .tabs[self.current_tab]
                .tree
                .find(focused_id)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            buffer_id = win.buffer;
        }
        let Some(buf) = self.pool.get(buffer_id) else {
            return Err(anyhow!("buffer {buffer_id:?} no longer in pool"));
        };
        let Buffer::Pdf(pdf) = buf else {
            // Explorer buffers have their own input path (see
            // render_loop). Navigator actions are a no-op here.
            return Ok(());
        };
        let doc = &pdf.pdf;
        let win = self
            .tabs[self.current_tab]
            .tree
            .find_mut(focused_id)
            .expect("focused window present");
        for _ in 0..count.max(1) {
            Navigator::apply(doc, &mut win.viewport, action.clone())?;
        }
        win.dirty = true;
        // Keep the buffer's sidecar state in sync with the viewport
        // the user is actually looking at, so a mid-session close /
        // `:edit` / crash doesn't lose zoom/page/night changes.
        self.sync_buffer_state_from_window(focused_id);
        Ok(())
    }

    /// Copy the given window's viewport back into its PDF buffer's
    /// `DocState`. No-op for explorer buffers or unknown windows.
    /// Searches all tabs so `persist_all` can fan out without having
    /// to flip the current-tab pointer.
    pub fn sync_buffer_state_from_window(&mut self, window: WindowId) {
        let found = self
            .tabs
            .iter()
            .find_map(|t| t.tree.find(window).map(|w| (w.buffer, w.viewport.clone())));
        let Some((buf_id, vp)) = found else {
            return;
        };
        let cache_enabled = self.cache.enabled();
        let (_, cache_cap) = self.cache.stats();
        let (_, ecache_cap) = self.ecache.stats();
        if let Some(Buffer::Pdf(pdf)) = self.pool.get_mut(buf_id) {
            pdf.state.last_page = vp.page_idx;
            pdf.state.zoom = vp.zoom;
            pdf.state.rotation = vp.rotation;
            pdf.state.scroll_x = vp.x_off;
            pdf.state.scroll_y = vp.y_off;
            pdf.state.night_mode = vp.night_mode;
            pdf.state.render_dpi = vp.render_dpi;
            pdf.state.render_quality = vp.render_quality;
            pdf.state.cache_enabled = cache_enabled;
            pdf.state.cache_size = Some(cache_cap);
            pdf.state.ecache_size = Some(ecache_cap);
        }
    }

    /// Apply the cache-size fields from a PDF buffer's sidecar to
    /// the workspace-global caches. Called whenever a new PDF buffer
    /// is opened — last-opened wins, matching the user's stated
    /// intent of "remember the setting in the sidecar."
    fn apply_cache_sizes_from(&mut self, buffer: BufferId) {
        let Some(Buffer::Pdf(pdf)) = self.pool.get(buffer) else {
            return;
        };
        if let Some(n) = pdf.state.cache_size {
            self.cache.resize(n);
        }
        if let Some(n) = pdf.state.ecache_size {
            self.ecache.resize(n);
        }
    }

    // -------------------- window-op dispatch --------------------

    pub fn apply_window_op(&mut self, op: WindowOp) -> Result<()> {
        match op {
            WindowOp::FocusLeft => self.focus_dir(Direction::Left),
            WindowOp::FocusRight => self.focus_dir(Direction::Right),
            WindowOp::FocusUp => self.focus_dir(Direction::Up),
            WindowOp::FocusDown => self.focus_dir(Direction::Down),
            WindowOp::FocusCycle { reverse } => {
                let focused = self.current_tab().focused;
                if let Some(next) = self.current_tab().tree.focus_cycle(focused, reverse) {
                    self.set_focus(next);
                }
                Ok(())
            }
            WindowOp::SplitHorizontal => self.split_focused(Axis::Horizontal, None),
            WindowOp::SplitVertical => self.split_focused(Axis::Vertical, None),
            WindowOp::Close => self.close_focused(),
            WindowOp::Only => self.only_focused(),
            WindowOp::Equalize => {
                self.current_tab_mut().tree.equalize();
                self.mark_all_dirty();
                Ok(())
            }
            WindowOp::ResizeVertical(n) => {
                self.resize_focused(Axis::Horizontal, n);
                Ok(())
            }
            WindowOp::ResizeHorizontal(n) => {
                self.resize_focused(Axis::Vertical, n);
                Ok(())
            }
            WindowOp::NextTab(n) => {
                self.goto_tab_rel(n as i32);
                Ok(())
            }
            WindowOp::PrevTab(n) => {
                self.goto_tab_rel(-(n as i32));
                Ok(())
            }
            WindowOp::MoveTabLeft => {
                self.move_current_tab(-1);
                Ok(())
            }
            WindowOp::MoveTabRight => {
                self.move_current_tab(1);
                Ok(())
            }
            WindowOp::AlternateBuffer => self.alternate_buffer(),
        }
    }

    fn focus_dir(&mut self, dir: Direction) -> Result<()> {
        let focused = self.current_tab().focused;
        let rect = self.last_rect;
        if let Some(next) = self
            .current_tab()
            .tree
            .focus_neighbour(focused, dir, rect)
        {
            self.set_focus(next);
        }
        Ok(())
    }

    fn set_focus(&mut self, id: WindowId) {
        let idx = self.current_tab;
        if self.tabs[idx].tree.find(id).is_some() {
            self.tabs[idx].focused = id;
            // Focus change → repaint status lines (dim vs bright).
            for w in self.tabs[idx].tree.windows_mut() {
                w.dirty = true;
            }
        }
    }

    fn split_focused(&mut self, axis: Axis, file: Option<&Path>) -> Result<()> {
        let focused_id = self.current_tab().focused;
        let (buffer_id, viewport_template) = {
            let win = self
                .current_tab()
                .tree
                .find(focused_id)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            (win.buffer, win.viewport.clone())
        };

        let new_buffer = match file {
            Some(p) => {
                let id = self.pool.open_pdf(&self.buf_ids, p, self.cache.clone())?;
                self.apply_cache_sizes_from(id);
                id
            }
            None => {
                // Share the current buffer. Bump its refcount so
                // closing one window doesn't tear down the doc.
                self.pool.acquire(buffer_id);
                buffer_id
            }
        };

        let new_viewport = if new_buffer == buffer_id {
            viewport_template
        } else {
            self.fresh_viewport_for(new_buffer, &viewport_template)
        };

        let new_id = self.win_ids.next();
        let new_window = Window::new(new_id, new_buffer, new_viewport);
        let idx = self.current_tab;
        let ok = self.tabs[idx].tree.split(focused_id, axis, new_window);
        if !ok {
            // Split failed — undo the refcount bump.
            self.pool.release(new_buffer);
            return Err(anyhow!("split: focused window not found in tree"));
        }
        self.tabs[idx].focused = new_id;
        self.mark_all_dirty();
        Ok(())
    }

    fn close_focused(&mut self) -> Result<()> {
        let focused_id = self.current_tab().focused;
        let idx = self.current_tab;
        let (released_buffer, outcome) = {
            let buffer_id = self.tabs[idx]
                .tree
                .find(focused_id)
                .map(|w| w.buffer)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            let outcome = self.tabs[idx].tree.close(focused_id);
            (buffer_id, outcome)
        };
        self.pool.release(released_buffer);
        match outcome {
            CloseOutcome::Closed { new_focus } => {
                self.tabs[idx].focused = new_focus;
                self.mark_all_dirty();
                Ok(())
            }
            CloseOutcome::LastWindow => {
                self.close_current_tab();
                Ok(())
            }
            CloseOutcome::NotFound => Err(anyhow!("close: focused window not found")),
        }
    }

    fn close_current_tab(&mut self) {
        // Release every buffer referenced by this tab.
        let buffers: Vec<BufferId> = self.tabs[self.current_tab]
            .tree
            .windows()
            .iter()
            .map(|w| w.buffer)
            .collect();
        for id in buffers {
            self.pool.release(id);
        }
        self.tabs.remove(self.current_tab);
        if self.tabs.is_empty() {
            self.quit_requested = true;
            return;
        }
        if self.current_tab >= self.tabs.len() {
            self.current_tab = self.tabs.len() - 1;
        }
        self.mark_all_dirty();
    }

    fn only_focused(&mut self) -> Result<()> {
        let idx = self.current_tab;
        let focused_id = self.tabs[idx].focused;
        // Collect all non-focused ids in this tab's tree, then close
        // each. Using `close` rather than blowing the tree away keeps
        // the buffer refcounts correct.
        let to_close: Vec<WindowId> = self.tabs[idx]
            .tree
            .windows()
            .iter()
            .map(|w| w.id)
            .filter(|id| *id != focused_id)
            .collect();
        for id in to_close {
            let buffer_id = self.tabs[idx]
                .tree
                .find(id)
                .map(|w| w.buffer)
                .unwrap_or(BufferId(0));
            let _ = self.tabs[idx].tree.close(id);
            if buffer_id.0 != 0 {
                self.pool.release(buffer_id);
            }
        }
        self.tabs[idx].focused = focused_id;
        self.mark_all_dirty();
        Ok(())
    }

    fn resize_focused(&mut self, axis: Axis, delta: i32) {
        let idx = self.current_tab;
        let focused_id = self.tabs[idx].focused;
        let total = match axis {
            Axis::Horizontal => self.last_rect.rows,
            Axis::Vertical => self.last_rect.cols,
        };
        self.tabs[idx].tree.resize(focused_id, axis, delta, total);
        self.mark_all_dirty();
    }

    fn alternate_buffer(&mut self) -> Result<()> {
        let idx = self.current_tab;
        let focused_id = self.tabs[idx].focused;
        let old_buffer = {
            let win = self.tabs[idx]
                .tree
                .find(focused_id)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            win.buffer
        };
        let new_buffer = {
            let win = self.tabs[idx].tree.find_mut(focused_id).unwrap();
            let Some(alt) = win.alternate else {
                return Err(anyhow!("no alternate buffer"));
            };
            win.alternate = Some(win.buffer);
            win.buffer = alt;
            win.dirty = true;
            win.buffer
        };
        self.pool.acquire(new_buffer);
        self.pool.release(old_buffer);
        // Seed the viewport from the new buffer's sidecar state.
        self.seed_viewport_from_buffer(focused_id, new_buffer);
        Ok(())
    }

    // -------------------- tab management --------------------

    fn goto_tab_rel(&mut self, delta: i32) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as i32;
        let cur = self.current_tab as i32;
        let next = ((cur + delta) % n + n) % n;
        self.current_tab = next as usize;
        self.mark_all_dirty();
    }

    /// Reorder the current tab in the tab list by `delta` positions.
    /// The focused tab stays the current one, just at a new index.
    fn move_current_tab(&mut self, delta: i32) {
        if self.tabs.len() <= 1 {
            return;
        }
        let n = self.tabs.len() as i32;
        let from = self.current_tab as i32;
        // Vim `:tabmove ±N` wraps around (tested behaviour on
        // `:tabmove +9` from the last tab).
        let to = ((from + delta) % n + n) % n;
        if to == from {
            return;
        }
        let tab = self.tabs.remove(from as usize);
        self.tabs.insert(to as usize, tab);
        self.current_tab = to as usize;
        self.mark_all_dirty();
    }

    fn new_tab_with(&mut self, buffer: BufferId, viewport_template: Viewport) -> Result<()> {
        self.pool.acquire(buffer);
        let vp = self.fresh_viewport_for(buffer, &viewport_template);
        let win_id = self.win_ids.next();
        let window = Window::new(win_id, buffer, vp);
        let tab = Tab::new(WindowTree::leaf(window));
        self.tabs.push(tab);
        self.current_tab = self.tabs.len() - 1;
        self.mark_all_dirty();
        Ok(())
    }

    // -------------------- command dispatch --------------------

    pub fn apply_command(&mut self, cmd: ParsedCommand) -> Result<()> {
        match cmd {
            ParsedCommand::Nav(action) => self.apply_nav(action, 1),
            ParsedCommand::Quit => {
                self.quit_requested = true;
                Ok(())
            }
            ParsedCommand::CloseWindow => {
                if self.tabs.len() == 1
                    && self.current_tab().tree.leaf_count() == 1
                {
                    // Last window → quit.
                    self.quit_requested = true;
                    Ok(())
                } else {
                    self.close_focused()
                }
            }
            ParsedCommand::OnlyWindow => self.only_focused(),
            ParsedCommand::Split { direction, file } => {
                let axis = match direction {
                    SplitDirection::Horizontal => Axis::Horizontal,
                    SplitDirection::Vertical => Axis::Vertical,
                };
                self.split_focused(axis, file.as_deref())
            }
            ParsedCommand::Edit(path) => self.edit_focused(&path),
            ParsedCommand::Explore { split, path } => self.explore(split, path),
            ParsedCommand::TabNew(file) => {
                let (buffer, template) = match file {
                    Some(p) => {
                        let buffer = self
                            .pool
                            .open_pdf(&self.buf_ids, &p, self.cache.clone())?;
                        self.apply_cache_sizes_from(buffer);
                        // New buffer — use focused window's viewport
                        // as screen-size template, with a fresh zoom
                        // state (seeded by buffer's sidecar).
                        let template = self.focused_window().viewport.clone();
                        (buffer, template)
                    }
                    None => {
                        let w = self.focused_window();
                        (w.buffer, w.viewport.clone())
                    }
                };
                self.new_tab_with(buffer, template)
            }
            ParsedCommand::TabClose => {
                self.close_current_tab();
                Ok(())
            }
            ParsedCommand::TabOnly => {
                if self.tabs.len() <= 1 {
                    return Ok(());
                }
                let keep = self.current_tab;
                // Release buffers in tabs we're removing.
                let to_release: Vec<BufferId> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != keep)
                    .flat_map(|(_, t)| t.tree.windows().into_iter().map(|w| w.buffer))
                    .collect();
                for id in to_release {
                    self.pool.release(id);
                }
                let saved = self.tabs.swap_remove(keep);
                self.tabs.clear();
                self.tabs.push(saved);
                self.current_tab = 0;
                self.mark_all_dirty();
                Ok(())
            }
            ParsedCommand::Buffer(n) => self.show_buffer_n(n),
            ParsedCommand::BufferNext => self.cycle_buffer(1),
            ParsedCommand::BufferPrev => self.cycle_buffer(-1),

            ParsedCommand::TabMove(n) => {
                self.move_current_tab(n);
                Ok(())
            }
            ParsedCommand::Resize(n) => {
                self.resize_focused(Axis::Horizontal, n);
                Ok(())
            }
            ParsedCommand::VResize(n) => {
                self.resize_focused(Axis::Vertical, n);
                Ok(())
            }
            ParsedCommand::Help
            | ParsedCommand::CacheSet(_)
            | ParsedCommand::CacheToggle
            | ParsedCommand::CacheSize(_)
            | ParsedCommand::ECacheSet(_)
            | ParsedCommand::ECacheToggle
            | ParsedCommand::ECacheSize(_)
            | ParsedCommand::Prefetch(_)
            | ParsedCommand::Reset
            | ParsedCommand::Colors(_)
            | ParsedCommand::CopyPage
            | ParsedCommand::ToggleToc
            | ParsedCommand::ToggleMarks
            | ParsedCommand::DeleteMark(_)
            | ParsedCommand::JumpBack
            | ParsedCommand::JumpForward
            | ParsedCommand::MouseSet(_)
            | ParsedCommand::MouseToggle
            | ParsedCommand::OpenTextEditor => {
                // These are handled by the render loop directly (they
                // touch UI state outside the workspace).
                Err(anyhow!("__workspace_passthrough__"))
            }
        }
    }

    fn edit_focused(&mut self, path: &Path) -> Result<()> {
        // `:edit` / `:open` on a directory drops the user into an
        // explorer there — the obvious thing to do, and the cheapest
        // way to let path completion bottom out at a dir.
        if path.is_dir() {
            return self.explore(None, Some(path.to_path_buf()));
        }
        let new_buf = self
            .pool
            .open_pdf(&self.buf_ids, path, self.cache.clone())?;
        self.apply_cache_sizes_from(new_buf);
        let idx = self.current_tab;
        let focused_id = self.tabs[idx].focused;
        let old_buf = {
            let win = self.tabs[idx]
                .tree
                .find_mut(focused_id)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            let old = win.buffer;
            if old == new_buf {
                // Reopening the same file is a no-op.
                self.pool.release(new_buf);
                return Ok(());
            }
            win.load(new_buf);
            old
        };
        self.pool.release(old_buf);
        self.seed_viewport_from_buffer(focused_id, new_buf);
        Ok(())
    }

    /// Open an explorer window. With `split = Some(_)`, creates the
    /// split first; with `None`, swaps the current window's buffer.
    fn explore(
        &mut self,
        split: Option<SplitDirection>,
        path: Option<PathBuf>,
    ) -> Result<()> {
        let cwd = match path {
            Some(p) => p,
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let new_buf = self.pool.open_explorer(&self.buf_ids, &cwd)?;
        match split {
            Some(dir) => {
                let axis = match dir {
                    SplitDirection::Horizontal => Axis::Horizontal,
                    SplitDirection::Vertical => Axis::Vertical,
                };
                // split_focused would re-open the buffer by path —
                // do the split by hand so we reuse the explorer we
                // just opened.
                let idx = self.current_tab;
                let focused_id = self.tabs[idx].focused;
                let viewport_template = self
                    .tabs[idx]
                    .tree
                    .find(focused_id)
                    .map(|w| w.viewport.clone())
                    .ok_or_else(|| anyhow!("focused window missing"))?;
                let new_id = self.win_ids.next();
                let new_window = Window::new(new_id, new_buf, viewport_template);
                let ok = self.tabs[idx].tree.split(focused_id, axis, new_window);
                if !ok {
                    self.pool.release(new_buf);
                    return Err(anyhow!("split: focused window not found in tree"));
                }
                self.tabs[idx].focused = new_id;
                self.mark_all_dirty();
                Ok(())
            }
            None => {
                let idx = self.current_tab;
                let focused_id = self.tabs[idx].focused;
                let old = {
                    let win = self.tabs[idx]
                        .tree
                        .find_mut(focused_id)
                        .ok_or_else(|| anyhow!("focused window missing"))?;
                    let old = win.buffer;
                    win.load(new_buf);
                    old
                };
                self.pool.release(old);
                self.mark_all_dirty();
                Ok(())
            }
        }
    }

    /// Called when the user hits `Enter`/`l` on an explorer entry.
    /// Descends into directories or opens the selected PDF into the
    /// current window.
    pub fn explorer_activate(&mut self) -> Result<()> {
        let focused_id = self.current_tab().focused;
        let buffer_id = {
            let win = self
                .current_tab()
                .tree
                .find(focused_id)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            win.buffer
        };
        let (target_path, is_dir) = {
            let Some(Buffer::Explorer(ex)) = self.pool.get(buffer_id) else {
                return Ok(());
            };
            let Some(entry) = ex.selected_entry() else {
                return Ok(());
            };
            let is_dir = entry.is_dir_like();
            let Some(path) = ex.selected_path() else {
                return Ok(());
            };
            (path, is_dir)
        };
        if is_dir {
            if let Some(Buffer::Explorer(ex)) = self.pool.get_mut(buffer_id) {
                ex.set_cwd(target_path)?;
            }
            let w = self
                .current_tab_mut()
                .tree
                .find_mut(focused_id)
                .expect("focused window present");
            w.dirty = true;
            Ok(())
        } else {
            // Swap the focused window's buffer to the PDF.
            let new_buf = self
                .pool
                .open_pdf(&self.buf_ids, &target_path, self.cache.clone())?;
            self.apply_cache_sizes_from(new_buf);
            let idx = self.current_tab;
            let old = {
                let win = self.tabs[idx]
                    .tree
                    .find_mut(focused_id)
                    .expect("focused window present");
                let old = win.buffer;
                win.load(new_buf);
                old
            };
            self.pool.release(old);
            self.seed_viewport_from_buffer(focused_id, new_buf);
            Ok(())
        }
    }

    /// `-` / `u` / `h` / Backspace inside an explorer.
    pub fn explorer_parent(&mut self) -> Result<()> {
        let id = self.focused_window().buffer;
        if let Some(Buffer::Explorer(ex)) = self.pool.get_mut(id) {
            ex.parent()?;
        }
        self.focused_window_mut().dirty = true;
        Ok(())
    }

    /// j/k and arrows in an explorer.
    pub fn explorer_move(&mut self, delta: isize) {
        let id = self.focused_window().buffer;
        if let Some(Buffer::Explorer(ex)) = self.pool.get_mut(id) {
            ex.move_selection(delta);
        }
        self.focused_window_mut().dirty = true;
    }

    pub fn explorer_first(&mut self) {
        let id = self.focused_window().buffer;
        if let Some(Buffer::Explorer(ex)) = self.pool.get_mut(id) {
            ex.goto_first();
        }
        self.focused_window_mut().dirty = true;
    }

    pub fn explorer_last(&mut self) {
        let id = self.focused_window().buffer;
        if let Some(Buffer::Explorer(ex)) = self.pool.get_mut(id) {
            ex.goto_last();
        }
        self.focused_window_mut().dirty = true;
    }

    fn show_buffer_n(&mut self, n: usize) -> Result<()> {
        let ids = self.pool.pdf_ids();
        let target = ids
            .iter()
            .find(|b| b.0 as usize == n)
            .copied()
            .or_else(|| ids.get(n.saturating_sub(1)).copied())
            .ok_or_else(|| anyhow!(":b {n}: no such buffer"))?;
        self.swap_focused_to(target)
    }

    fn cycle_buffer(&mut self, dir: i32) -> Result<()> {
        let ids = self.pool.pdf_ids();
        if ids.is_empty() {
            return Err(anyhow!("no open buffers"));
        }
        let current = self.focused_window().buffer;
        let pos = ids.iter().position(|i| *i == current).unwrap_or(0) as i32;
        let n = ids.len() as i32;
        let next_idx = ((pos + dir) % n + n) % n;
        let target = ids[next_idx as usize];
        self.swap_focused_to(target)
    }

    fn swap_focused_to(&mut self, target: BufferId) -> Result<()> {
        let idx = self.current_tab;
        let focused_id = self.tabs[idx].focused;
        let old = {
            let win = self.tabs[idx]
                .tree
                .find_mut(focused_id)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            if win.buffer == target {
                return Ok(());
            }
            let old = win.buffer;
            win.load(target);
            old
        };
        self.pool.acquire(target);
        self.pool.release(old);
        self.seed_viewport_from_buffer(focused_id, target);
        Ok(())
    }

    /// Seed a window's viewport from the target buffer's persisted
    /// DocState, preserving the current screen dims. Only meaningful
    /// for PDF buffers — explorer buffers leave the viewport alone.
    fn seed_viewport_from_buffer(&mut self, window: WindowId, buffer: BufferId) {
        let Some(pdf) = self.pool.get_pdf(buffer) else {
            return;
        };
        let state = pdf.state.clone();
        let page_count = pdf.pdf.page_count();
        let idx = self.current_tab;
        let Some(win) = self.tabs[idx].tree.find_mut(window) else {
            return;
        };
        let prev_w = win.viewport.screen_w;
        let prev_h = win.viewport.screen_h;
        win.viewport = Viewport {
            page_idx: state.last_page.min(page_count.saturating_sub(1)),
            x_off: state.scroll_x,
            y_off: state.scroll_y,
            zoom: state.zoom,
            rotation: state.rotation,
            screen_w: prev_w,
            screen_h: prev_h,
            night_mode: state.night_mode,
            render_dpi: state.render_dpi,
            render_quality: state.render_quality,
        };
        win.dirty = true;
    }

    fn fresh_viewport_for(&self, buffer: BufferId, template: &Viewport) -> Viewport {
        let Some(pdf) = self.pool.get_pdf(buffer) else {
            return template.clone();
        };
        let state = &pdf.state;
        let page_count = pdf.pdf.page_count();
        Viewport {
            page_idx: state.last_page.min(page_count.saturating_sub(1)),
            x_off: state.scroll_x,
            y_off: state.scroll_y,
            zoom: state.zoom,
            rotation: state.rotation,
            screen_w: template.screen_w,
            screen_h: template.screen_h,
            night_mode: state.night_mode,
            render_dpi: state.render_dpi,
            render_quality: state.render_quality,
        }
    }

    // -------------------- M2: jump list / marks / mouse / links --------------------

    /// Set the focused window's `WindowId` directly. Used by mouse
    /// click-to-focus.
    pub fn set_focus_window(&mut self, id: WindowId) {
        self.set_focus(id);
    }

    /// Capture the focused window's current viewport as a JumpEntry on
    /// its buffer's back-stack. Clears the forward-stack — vim's
    /// `<C-o>` semantics: making a new jump invalidates the old
    /// forward branch.
    fn push_back_stack(&mut self) {
        let win = self.focused_window();
        let buf_id = win.buffer;
        let entry = JumpEntry {
            page_idx: win.viewport.page_idx,
            x_off: win.viewport.x_off,
            y_off: win.viewport.y_off,
        };
        if let Some(pdf) = self.pool.get_pdf_mut(buf_id) {
            // Cap at 100 — matches vim's default jumplist size. Pop
            // the oldest entry when over.
            const CAP: usize = 100;
            pdf.back_stack.push(entry);
            if pdf.back_stack.len() > CAP {
                pdf.back_stack.remove(0);
            }
            pdf.forward_stack.clear();
        }
    }

    /// Jump to a target page+offset, pushing the current viewport
    /// onto the back-stack first. Used by TOC, marks, and link-follow.
    pub fn jump_to_page(&mut self, page: usize, x_off: i32, y_off: i32) -> Result<()> {
        self.push_back_stack();
        self.apply_nav(
            Action::JumpTo {
                page_idx: page,
                x_off,
                y_off,
            },
            1,
        )
    }

    /// `<C-o>`: pop the back-stack and jump there, pushing the
    /// current viewport onto the forward-stack. Returns Ok(false) if
    /// the back-stack was empty.
    pub fn jump_back(&mut self) -> Result<bool> {
        let buf_id = self.focused_window().buffer;
        let popped = match self.pool.get_pdf_mut(buf_id) {
            Some(pdf) => pdf.back_stack.pop(),
            None => return Ok(false),
        };
        let Some(entry) = popped else { return Ok(false) };
        // Capture current state to forward-stack.
        let cur_entry = {
            let win = self.focused_window();
            JumpEntry {
                page_idx: win.viewport.page_idx,
                x_off: win.viewport.x_off,
                y_off: win.viewport.y_off,
            }
        };
        if let Some(pdf) = self.pool.get_pdf_mut(buf_id) {
            pdf.forward_stack.push(cur_entry);
        }
        self.apply_nav(
            Action::JumpTo {
                page_idx: entry.page_idx,
                x_off: entry.x_off,
                y_off: entry.y_off,
            },
            1,
        )?;
        Ok(true)
    }

    /// Counterpart to `jump_back`. `<C-i>` (rare in terminals) or
    /// `:forward`.
    pub fn jump_forward(&mut self) -> Result<bool> {
        let buf_id = self.focused_window().buffer;
        let popped = match self.pool.get_pdf_mut(buf_id) {
            Some(pdf) => pdf.forward_stack.pop(),
            None => return Ok(false),
        };
        let Some(entry) = popped else { return Ok(false) };
        let cur_entry = {
            let win = self.focused_window();
            JumpEntry {
                page_idx: win.viewport.page_idx,
                x_off: win.viewport.x_off,
                y_off: win.viewport.y_off,
            }
        };
        if let Some(pdf) = self.pool.get_pdf_mut(buf_id) {
            pdf.back_stack.push(cur_entry);
        }
        self.apply_nav(
            Action::JumpTo {
                page_idx: entry.page_idx,
                x_off: entry.x_off,
                y_off: entry.y_off,
            },
            1,
        )?;
        Ok(true)
    }

    /// Set a mark from the focused viewport's current position.
    pub fn set_bookmark(&mut self, mark: char) -> Result<()> {
        if !mark.is_ascii_alphabetic() {
            return Err(anyhow!("mark must be a letter"));
        }
        let win = self.focused_window();
        let buf_id = win.buffer;
        let page = win.viewport.page_idx;
        let x_off = win.viewport.x_off;
        let y_off = win.viewport.y_off;
        let Some(pdf) = self.pool.get_pdf_mut(buf_id) else {
            return Err(anyhow!("not a PDF buffer"));
        };
        pdf.state.set_bookmark(mark, page, x_off, y_off);
        Ok(())
    }

    /// `'{a-z}` — recall a mark. Returns Ok(false) if the mark wasn't
    /// set. Records the pre-jump viewport to the back-stack.
    pub fn jump_bookmark(&mut self, mark: char) -> Result<bool> {
        let buf_id = self.focused_window().buffer;
        let target = match self.pool.get_pdf(buf_id) {
            Some(pdf) => pdf.state.find_bookmark(mark).copied(),
            None => return Ok(false),
        };
        let Some(bm) = target else { return Ok(false) };
        self.jump_to_page(bm.page, bm.x_off, bm.y_off)?;
        Ok(true)
    }

    pub fn delete_bookmark(&mut self, mark: char) -> bool {
        let buf_id = self.focused_window().buffer;
        match self.pool.get_pdf_mut(buf_id) {
            Some(pdf) => pdf.state.delete_bookmark(mark),
            None => false,
        }
    }

    /// Persist mouse-on/off in the focused buffer's DocState. The
    /// terminal-side capture is toggled by the caller.
    pub fn set_mouse_pref(&mut self, enabled: Option<bool>) {
        let buf_id = self.focused_window().buffer;
        if let Some(pdf) = self.pool.get_pdf_mut(buf_id) {
            pdf.state.mouse_enabled = enabled;
        }
    }

    /// Hit-test a click in window `id` at window-relative pixel
    /// (x, y). On hit, follows the link (jump + back-stack push).
    /// Returns Ok(()) whether or not a link was hit; callers don't
    /// get to know which case happened (matches vim — link follow is
    /// silent, miss is silent).
    pub fn click_at(&mut self, id: WindowId, x: i32, y: i32) -> Result<()> {
        let buf_id = {
            let Some(win) = self.tabs[self.current_tab].tree.find(id) else {
                return Ok(());
            };
            win.buffer
        };
        let (viewport, page_idx, page_size) = {
            let win = self.tabs[self.current_tab]
                .tree
                .find(id)
                .ok_or_else(|| anyhow!("focused window missing"))?;
            let Some(pdf) = self.pool.get_pdf(buf_id) else {
                return Ok(());
            };
            let page_idx = win.viewport.page_idx;
            let page_size = pdf.pdf.page_size(page_idx)?;
            (win.viewport.clone(), page_idx, page_size)
        };
        let Some((px, py)) = viewport.screen_to_pdf_point(page_size, x, y) else {
            return Ok(());
        };
        let target = {
            let Some(pdf_buf) = self.pool.get_pdf_mut(buf_id) else {
                return Ok(());
            };
            let links = pdf_buf.links_for(page_idx);
            link_hit(links, px, py)
        };
        let Some((dest_page, dest_point)) = target else {
            return Ok(());
        };
        // Translate dest_point (PDF user-space) into a viewport
        // (x_off, y_off) for the dest page. If the destination has no
        // explicit point, land at top-left.
        let (x_off, y_off) = match dest_point {
            None => (0, 0),
            Some((px, py)) => {
                // Reuse the focused window's viewport zoom/rotation
                // to compute the dest scroll offsets.
                let dest_size = match self.pool.get_pdf(buf_id) {
                    Some(pdf) => pdf.pdf.page_size(dest_page)?,
                    None => return Ok(()),
                };
                let mut vp = viewport.clone();
                vp.page_idx = dest_page;
                let scale = vp.display_scale(dest_size);
                let (rx, ry) = match vp.rotation {
                    svreader_core::Rotation::R0 => (px, py),
                    svreader_core::Rotation::R90 => (dest_size.height - py, px),
                    svreader_core::Rotation::R180 => {
                        (dest_size.width - px, dest_size.height - py)
                    }
                    svreader_core::Rotation::R270 => (py, dest_size.width - px),
                };
                let x_off = (rx * scale).round() as i32;
                let y_off = (ry * scale).round() as i32;
                (x_off, y_off)
            }
        };
        self.jump_to_page(dest_page, x_off, y_off)?;
        Ok(())
    }

    /// Flush every open buffer's DocState to disk. Called at shutdown
    /// and on tab/buffer close. Explorer buffers have no state to save.
    pub fn persist_all(&mut self) {
        // Sync every window's viewport back into its buffer first.
        // For shared buffers this is last-writer-wins, which matches
        // vim's single-sidecar-per-file model.
        let window_ids: Vec<WindowId> = self
            .tabs
            .iter()
            .flat_map(|t| t.tree.windows().into_iter().map(|w| w.id))
            .collect();
        for id in window_ids {
            self.sync_buffer_state_from_window(id);
        }
        for id in self.pool.ids() {
            if let Some(Buffer::Pdf(pdf)) = self.pool.get_mut(id) {
                if let Err(e) = pdf.state.save(&pdf.path) {
                    tracing::warn!("failed to save sidecar for {:?}: {e:#}", pdf.path);
                }
            }
        }
    }
}

/// Is this navigation action a "jump motion" worth pushing onto the
/// back-stack? Mirrors vim's jumplist policy — only long-distance
/// moves count, not continuous scrolling.
fn is_jump_action(action: &Action) -> bool {
    matches!(
        action,
        Action::GotoPage(_) | Action::FirstPage | Action::LastPage
    )
}

/// Find the link whose bounds contain the PDF user-space point (x, y).
/// Returns the destination page + optional dest point. `None` if no
/// hit. Used by mouse click-to-follow.
fn link_hit(links: &[PageLink], x: f32, y: f32) -> Option<(usize, Option<(f32, f32)>)> {
    links
        .iter()
        .find(|l| l.bounds.contains(x, y))
        .map(|l| (l.dest_page, l.dest_point))
}

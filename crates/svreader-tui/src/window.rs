//! Vim-style split tree.
//!
//! A `Tab` owns a `WindowTree`. Each leaf is a `Window` that holds
//! a `BufferId` and its own `Viewport`. Two windows with the same
//! `BufferId` form a "shared buffer" — two viewports on one PDF, vim-
//! style.
//!
//! The tree is split-only: every internal node is a 50/50-ish
//! horizontal or vertical split; ratios can be nudged by
//! `<C-w>+/-/>/<` or reset by `<C-w>=`. Layout uses simple cell
//! arithmetic so every split edge lands on a cell boundary — sixel
//! images can safely fill their window rect without tearing.

use std::sync::atomic::{AtomicU32, Ordering};

use svreader_core::{BufferId, Viewport};

use crate::sixel_write::ColorMode;
use crate::timings::FrameTiming;

/// Per-window identifier. u32 so it fits in terminal-facing state
/// cheaply and never wraps in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u32);

#[derive(Debug, Default)]
pub struct WindowIdSource {
    next: AtomicU32,
}

impl WindowIdSource {
    pub fn new() -> Self {
        Self {
            next: AtomicU32::new(1),
        }
    }

    pub fn next(&self) -> WindowId {
        WindowId(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

/// A cell-aligned rectangle in the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

impl CellRect {
    pub fn right(&self) -> u16 {
        self.col.saturating_add(self.cols)
    }
    pub fn bottom(&self) -> u16 {
        self.row.saturating_add(self.rows)
    }
    pub fn is_empty(&self) -> bool {
        self.cols == 0 || self.rows == 0
    }
}

/// One open view — a viewport onto a buffer.
pub struct Window {
    pub id: WindowId,
    /// Which buffer this window is showing. Multiple windows may
    /// share a `BufferId`.
    pub buffer: BufferId,
    /// Last `BufferId` this window was showing — `<C-^>` swaps to it.
    pub alternate: Option<BufferId>,
    pub viewport: Viewport,
    pub color_mode: ColorMode,

    // Per-window status + repaint state.
    pub last_timing: Option<FrameTiming>,
    pub last_dpi: f32,
    pub last_sixel_rows: u16,
    /// Rect of the last paint. `None` before first paint.
    pub last_rect: Option<CellRect>,
    /// Force a re-render (cache hit still OK).
    pub dirty: bool,
}

impl Window {
    pub fn new(id: WindowId, buffer: BufferId, viewport: Viewport) -> Self {
        Self {
            id,
            buffer,
            alternate: None,
            viewport,
            color_mode: ColorMode::XTerm256,
            last_timing: None,
            last_dpi: 72.0,
            last_sixel_rows: 0,
            last_rect: None,
            dirty: true,
        }
    }

    /// Point this window at a new buffer, stashing the old one as
    /// alternate.
    pub fn load(&mut self, buffer: BufferId) {
        if self.buffer != buffer {
            self.alternate = Some(self.buffer);
            self.buffer = buffer;
        }
        self.dirty = true;
    }
}

/// Split orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Horizontal split: top / bottom.
    Horizontal,
    /// Vertical split: left / right.
    Vertical,
}

/// Direction for focus moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Recursive split tree.
pub enum WindowTree {
    Leaf(Window),
    Split {
        axis: Axis,
        /// First child: top (Horizontal) or left (Vertical).
        a: Box<WindowTree>,
        /// Second child: bottom (Horizontal) or right (Vertical).
        b: Box<WindowTree>,
        /// Fraction of the total range that `a` gets (0.05..0.95).
        ratio: f32,
    },
}

impl WindowTree {
    pub fn leaf(window: Window) -> Self {
        WindowTree::Leaf(window)
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            WindowTree::Leaf(_) => 1,
            WindowTree::Split { a, b, .. } => a.leaf_count() + b.leaf_count(),
        }
    }

    pub fn find(&self, id: WindowId) -> Option<&Window> {
        match self {
            WindowTree::Leaf(w) => (w.id == id).then_some(w),
            WindowTree::Split { a, b, .. } => a.find(id).or_else(|| b.find(id)),
        }
    }

    pub fn find_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        match self {
            WindowTree::Leaf(w) => {
                if w.id == id {
                    Some(w)
                } else {
                    None
                }
            }
            WindowTree::Split { a, b, .. } => a.find_mut(id).or_else(|| b.find_mut(id)),
        }
    }

    pub fn windows(&self) -> Vec<&Window> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    pub fn windows_mut(&mut self) -> Vec<&mut Window> {
        let mut out: Vec<&mut Window> = Vec::new();
        fn walk<'a>(node: &'a mut WindowTree, out: &mut Vec<&'a mut Window>) {
            match node {
                WindowTree::Leaf(w) => out.push(w),
                WindowTree::Split { a, b, .. } => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        walk(self, &mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a Window>) {
        match self {
            WindowTree::Leaf(w) => out.push(w),
            WindowTree::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// Cell-rect layout for every leaf.
    pub fn layout(&self, rect: CellRect) -> Vec<(WindowId, CellRect)> {
        let mut out = Vec::new();
        layout_into(self, rect, &mut out);
        out
    }

    /// Insert a split on the leaf identified by `id`. The existing
    /// leaf stays on the `a` side; `new_window` becomes the `b` side
    /// (bottom for horizontal, right for vertical). Matches vim's
    /// `splitbelow` / `splitright` behaviour — new content appears
    /// down/right of the current window.
    ///
    /// Returns `true` if the split was applied.
    pub fn split(&mut self, id: WindowId, axis: Axis, new_window: Window) -> bool {
        let mut new_window = Some(new_window);
        split_at(self, id, axis, &mut new_window);
        new_window.is_none()
    }

    /// Remove the leaf identified by `id`. Returns the new focus
    /// target (its nearest remaining leaf) if the tree is still
    /// non-empty; returns `None` when removing the last leaf — the
    /// caller is expected to close the tab in that case.
    pub fn close(&mut self, id: WindowId) -> CloseOutcome {
        // First, a simple check: are we the lone leaf being closed?
        if let WindowTree::Leaf(w) = self {
            if w.id == id {
                return CloseOutcome::LastWindow;
            } else {
                return CloseOutcome::NotFound;
            }
        }
        match close_in(self, id) {
            CloseResult::NotFound => CloseOutcome::NotFound,
            CloseResult::Removed(new_focus) => CloseOutcome::Closed { new_focus },
        }
    }

    /// Reset all split ratios to 50/50.
    pub fn equalize(&mut self) {
        match self {
            WindowTree::Leaf(_) => {}
            WindowTree::Split { a, b, ratio, .. } => {
                *ratio = 0.5;
                a.equalize();
                b.equalize();
            }
        }
    }

    /// Adjust the ratio of the nearest ancestor split with the
    /// matching axis. `delta` is in cell-units, converted into a
    /// ratio nudge based on `total` along the resized axis.
    pub fn resize(&mut self, id: WindowId, axis: Axis, delta_cells: i32, total: u16) {
        if total == 0 {
            return;
        }
        let nudge = (delta_cells as f32) / (total as f32);
        // Walk down to `id`, then back up adjusting the first
        // ancestor whose axis matches.
        resize_walk(self, id, axis, nudge);
    }

    /// Pick the nearest leaf in `dir` relative to the focused window.
    /// Returns `None` if `focused` isn't in the tree or no neighbour
    /// exists in that direction.
    pub fn focus_neighbour(
        &self,
        focused: WindowId,
        dir: Direction,
        rect: CellRect,
    ) -> Option<WindowId> {
        let layout = self.layout(rect);
        let focused_rect = layout.iter().find(|(id, _)| *id == focused)?.1;
        let mut best: Option<(WindowId, i32, i32)> = None;

        for (id, r) in &layout {
            if *id == focused {
                continue;
            }
            let (primary, overlap) = match dir {
                Direction::Left => {
                    if r.right() > focused_rect.col {
                        continue;
                    }
                    let gap = focused_rect.col as i32 - r.right() as i32;
                    let ov = row_overlap(r, &focused_rect);
                    (gap, ov)
                }
                Direction::Right => {
                    if r.col < focused_rect.right() {
                        continue;
                    }
                    let gap = r.col as i32 - focused_rect.right() as i32;
                    let ov = row_overlap(r, &focused_rect);
                    (gap, ov)
                }
                Direction::Up => {
                    if r.bottom() > focused_rect.row {
                        continue;
                    }
                    let gap = focused_rect.row as i32 - r.bottom() as i32;
                    let ov = col_overlap(r, &focused_rect);
                    (gap, ov)
                }
                Direction::Down => {
                    if r.row < focused_rect.bottom() {
                        continue;
                    }
                    let gap = r.row as i32 - focused_rect.bottom() as i32;
                    let ov = col_overlap(r, &focused_rect);
                    (gap, ov)
                }
            };
            if overlap <= 0 {
                continue;
            }
            match best {
                None => best = Some((*id, primary, overlap)),
                Some((_, b_primary, b_overlap)) => {
                    if primary < b_primary
                        || (primary == b_primary && overlap > b_overlap)
                    {
                        best = Some((*id, primary, overlap));
                    }
                }
            }
        }
        best.map(|(id, _, _)| id)
    }

    /// Cycle focus through leaves in tree order. `reverse=true`
    /// goes the other way.
    pub fn focus_cycle(&self, focused: WindowId, reverse: bool) -> Option<WindowId> {
        let ids: Vec<WindowId> = self.windows().into_iter().map(|w| w.id).collect();
        if ids.is_empty() {
            return None;
        }
        let idx = ids.iter().position(|id| *id == focused)?;
        let next = if reverse {
            (idx + ids.len() - 1) % ids.len()
        } else {
            (idx + 1) % ids.len()
        };
        Some(ids[next])
    }

    /// First leaf, used after `:only` etc.
    pub fn first_id(&self) -> WindowId {
        match self {
            WindowTree::Leaf(w) => w.id,
            WindowTree::Split { a, .. } => a.first_id(),
        }
    }
}

pub enum CloseOutcome {
    /// Tree still has leaves; focus should move to `new_focus`.
    Closed { new_focus: WindowId },
    /// The closed leaf was the last in the tree. Caller should close
    /// its tab.
    LastWindow,
    NotFound,
}

enum CloseResult {
    NotFound,
    Removed(WindowId),
}

fn layout_into(node: &WindowTree, rect: CellRect, out: &mut Vec<(WindowId, CellRect)>) {
    if rect.is_empty() {
        return;
    }
    match node {
        WindowTree::Leaf(w) => out.push((w.id, rect)),
        WindowTree::Split { axis, a, b, ratio } => {
            let r = ratio.clamp(0.05, 0.95);
            match axis {
                Axis::Horizontal => {
                    let a_rows = ((rect.rows as f32) * r).round() as u16;
                    let a_rows = a_rows.clamp(1, rect.rows.saturating_sub(1).max(1));
                    let a_rect = CellRect {
                        col: rect.col,
                        row: rect.row,
                        cols: rect.cols,
                        rows: a_rows,
                    };
                    let b_rect = CellRect {
                        col: rect.col,
                        row: rect.row + a_rows,
                        cols: rect.cols,
                        rows: rect.rows - a_rows,
                    };
                    layout_into(a, a_rect, out);
                    layout_into(b, b_rect, out);
                }
                Axis::Vertical => {
                    let a_cols = ((rect.cols as f32) * r).round() as u16;
                    let a_cols = a_cols.clamp(1, rect.cols.saturating_sub(1).max(1));
                    let a_rect = CellRect {
                        col: rect.col,
                        row: rect.row,
                        cols: a_cols,
                        rows: rect.rows,
                    };
                    let b_rect = CellRect {
                        col: rect.col + a_cols,
                        row: rect.row,
                        cols: rect.cols - a_cols,
                        rows: rect.rows,
                    };
                    layout_into(a, a_rect, out);
                    layout_into(b, b_rect, out);
                }
            }
        }
    }
}

/// Mutable descent: replace the `Leaf(id)` node with a Split
/// containing (old_leaf, new_window).
fn split_at(
    node: &mut WindowTree,
    id: WindowId,
    axis: Axis,
    new_window: &mut Option<Window>,
) {
    match node {
        WindowTree::Leaf(w) if w.id == id => {
            // Swap the leaf out, keep it as `a`, put the new window
            // on `b`.
            let Some(new) = new_window.take() else {
                return;
            };
            let dummy_id = WindowId(0);
            let placeholder = Window {
                id: dummy_id,
                buffer: w.buffer,
                alternate: None,
                viewport: w.viewport.clone(),
                color_mode: w.color_mode,
                last_timing: None,
                last_dpi: 0.0,
                last_sixel_rows: 0,
                last_rect: None,
                dirty: true,
            };
            let old = std::mem::replace(w, placeholder);
            *node = WindowTree::Split {
                axis,
                a: Box::new(WindowTree::Leaf(old)),
                b: Box::new(WindowTree::Leaf(new)),
                ratio: 0.5,
            };
        }
        WindowTree::Leaf(_) => {}
        WindowTree::Split { a, b, .. } => {
            split_at(a, id, axis, new_window);
            if new_window.is_some() {
                split_at(b, id, axis, new_window);
            }
        }
    }
}

fn close_in(node: &mut WindowTree, id: WindowId) -> CloseResult {
    // The only interesting case at a Leaf is NotFound — we already
    // short-circuited the single-leaf case at the public entry point.
    match node {
        WindowTree::Leaf(_) => CloseResult::NotFound,
        WindowTree::Split { a, b, .. } => {
            // Look at each child. If a child is a Leaf matching `id`,
            // replace the whole Split with the sibling subtree.
            if let WindowTree::Leaf(w) = a.as_ref() {
                if w.id == id {
                    let sibling = std::mem::replace(
                        b.as_mut(),
                        WindowTree::Leaf(Window::new(
                            WindowId(0),
                            w.buffer,
                            w.viewport.clone(),
                        )),
                    );
                    *node = sibling;
                    let focus = node.first_id();
                    return CloseResult::Removed(focus);
                }
            }
            if let WindowTree::Leaf(w) = b.as_ref() {
                if w.id == id {
                    let sibling = std::mem::replace(
                        a.as_mut(),
                        WindowTree::Leaf(Window::new(
                            WindowId(0),
                            w.buffer,
                            w.viewport.clone(),
                        )),
                    );
                    *node = sibling;
                    let focus = node.first_id();
                    return CloseResult::Removed(focus);
                }
            }
            // Neither direct child matched; recurse.
            match close_in(a, id) {
                CloseResult::Removed(f) => return CloseResult::Removed(f),
                CloseResult::NotFound => {}
            }
            close_in(b, id)
        }
    }
}

/// Walk to the leaf with `id`, then on unwind adjust the first
/// ancestor split that matches `axis`.
fn resize_walk(node: &mut WindowTree, id: WindowId, axis: Axis, nudge: f32) -> ResizeUnwind {
    match node {
        WindowTree::Leaf(w) => {
            if w.id == id {
                ResizeUnwind::LookingForAncestor
            } else {
                ResizeUnwind::NotFound
            }
        }
        WindowTree::Split {
            axis: this_axis,
            a,
            b,
            ratio,
        } => {
            let down = resize_walk(a, id, axis, nudge);
            let down = match down {
                ResizeUnwind::NotFound => resize_walk(b, id, axis, nudge),
                other => other,
            };
            match down {
                ResizeUnwind::LookingForAncestor if *this_axis == axis => {
                    *ratio = (*ratio + nudge).clamp(0.1, 0.9);
                    ResizeUnwind::Done
                }
                other => other,
            }
        }
    }
}

enum ResizeUnwind {
    NotFound,
    LookingForAncestor,
    Done,
}

fn row_overlap(a: &CellRect, b: &CellRect) -> i32 {
    let top = a.row.max(b.row);
    let bot = a.bottom().min(b.bottom());
    (bot as i32 - top as i32).max(0)
}

fn col_overlap(a: &CellRect, b: &CellRect) -> i32 {
    let left = a.col.max(b.col);
    let right = a.right().min(b.right());
    (right as i32 - left as i32).max(0)
}

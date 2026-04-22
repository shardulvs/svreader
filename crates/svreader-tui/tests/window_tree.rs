//! WindowTree layout / split / focus-move / close invariants.

use svreader_core::{BufferId, Viewport};
use svreader_tui::window::{
    Axis, CellRect, CloseOutcome, Direction, Window, WindowId, WindowTree,
};

fn leaf(id: u32, buf: u32) -> WindowTree {
    WindowTree::leaf(Window::new(
        WindowId(id),
        BufferId(buf),
        Viewport::default(),
    ))
}

fn rect(cols: u16, rows: u16) -> CellRect {
    CellRect {
        col: 0,
        row: 0,
        cols,
        rows,
    }
}

#[test]
fn single_leaf_layout_fills_rect() {
    let t = leaf(1, 1);
    let out = t.layout(rect(80, 24));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, WindowId(1));
    assert_eq!(out[0].1.cols, 80);
    assert_eq!(out[0].1.rows, 24);
}

#[test]
fn vertical_split_halves_cols() {
    let mut t = leaf(1, 1);
    let new = Window::new(WindowId(2), BufferId(1), Viewport::default());
    assert!(t.split(WindowId(1), Axis::Vertical, new));
    let out = t.layout(rect(80, 24));
    assert_eq!(out.len(), 2);
    // Left is the original leaf (id=1), right is the new window (id=2).
    let left = out.iter().find(|(id, _)| *id == WindowId(1)).unwrap().1;
    let right = out.iter().find(|(id, _)| *id == WindowId(2)).unwrap().1;
    assert_eq!(left.col, 0);
    assert_eq!(right.col, left.cols);
    assert_eq!(left.cols + right.cols, 80);
    assert_eq!(left.rows, 24);
    assert_eq!(right.rows, 24);
}

#[test]
fn horizontal_split_halves_rows() {
    let mut t = leaf(1, 1);
    let new = Window::new(WindowId(2), BufferId(1), Viewport::default());
    assert!(t.split(WindowId(1), Axis::Horizontal, new));
    let out = t.layout(rect(80, 24));
    let top = out.iter().find(|(id, _)| *id == WindowId(1)).unwrap().1;
    let bottom = out.iter().find(|(id, _)| *id == WindowId(2)).unwrap().1;
    assert_eq!(top.row, 0);
    assert_eq!(bottom.row, top.rows);
    assert_eq!(top.rows + bottom.rows, 24);
}

#[test]
fn focus_neighbour_moves_to_adjacent_split() {
    let mut t = leaf(1, 1);
    t.split(
        WindowId(1),
        Axis::Vertical,
        Window::new(WindowId(2), BufferId(1), Viewport::default()),
    );
    let r = rect(80, 24);
    // Focused = 1 (left), l moves to 2 (right).
    assert_eq!(
        t.focus_neighbour(WindowId(1), Direction::Right, r),
        Some(WindowId(2))
    );
    assert_eq!(
        t.focus_neighbour(WindowId(2), Direction::Left, r),
        Some(WindowId(1))
    );
    // No neighbour above.
    assert!(t.focus_neighbour(WindowId(1), Direction::Up, r).is_none());
}

#[test]
fn focus_cycle_wraps() {
    let mut t = leaf(1, 1);
    t.split(
        WindowId(1),
        Axis::Vertical,
        Window::new(WindowId(2), BufferId(1), Viewport::default()),
    );
    t.split(
        WindowId(2),
        Axis::Horizontal,
        Window::new(WindowId(3), BufferId(1), Viewport::default()),
    );
    assert_eq!(t.leaf_count(), 3);
    let first = t.focus_cycle(WindowId(1), false).unwrap();
    assert_ne!(first, WindowId(1));
    let wrap = t.focus_cycle(WindowId(3), false);
    assert!(wrap.is_some());
}

#[test]
fn close_leaves_single_window() {
    let mut t = leaf(1, 1);
    t.split(
        WindowId(1),
        Axis::Vertical,
        Window::new(WindowId(2), BufferId(1), Viewport::default()),
    );
    let out = t.close(WindowId(2));
    match out {
        CloseOutcome::Closed { new_focus } => assert_eq!(new_focus, WindowId(1)),
        _ => panic!("wrong outcome"),
    }
    assert_eq!(t.leaf_count(), 1);
    let layout = t.layout(rect(80, 24));
    assert_eq!(layout.len(), 1);
    assert_eq!(layout[0].0, WindowId(1));
    assert_eq!(layout[0].1.cols, 80);
}

#[test]
fn close_last_window_reports_last() {
    let mut t = leaf(1, 1);
    let out = t.close(WindowId(1));
    assert!(matches!(out, CloseOutcome::LastWindow));
}

#[test]
fn equalize_resets_ratio() {
    let mut t = leaf(1, 1);
    t.split(
        WindowId(1),
        Axis::Vertical,
        Window::new(WindowId(2), BufferId(1), Viewport::default()),
    );
    // Nudge the split by resize, then equalize.
    t.resize(WindowId(1), Axis::Vertical, 10, 80);
    let before = t.layout(rect(80, 24));
    let before_left = before.iter().find(|(id, _)| *id == WindowId(1)).unwrap().1;
    t.equalize();
    let after = t.layout(rect(80, 24));
    let after_left = after.iter().find(|(id, _)| *id == WindowId(1)).unwrap().1;
    // After equalize, left should be ~40 cols; before resize it was ~50.
    assert!(after_left.cols != before_left.cols);
    assert!((after_left.cols as i32 - 40).abs() <= 1);
}

//! `ExplorerBuffer` unit tests. Build small fixture directories on
//! disk, open them through `ExplorerBuffer::open`, and exercise the
//! selection / filter / parent / descend pipeline headlessly.

use std::fs;

use svreader_core::{BufferId, ExplorerBuffer, ExplorerKind};

fn tempdir(name: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let d = std::env::temp_dir().join(format!("svreader-explorer-{name}-{pid}-{ts}"));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Build a canonical fixture tree and return the root.
fn make_fixture() -> std::path::PathBuf {
    let root = tempdir("root");
    // Visible dirs.
    fs::create_dir_all(root.join("alpha")).unwrap();
    fs::create_dir_all(root.join("beta")).unwrap();
    // Hidden dir — must not appear.
    fs::create_dir_all(root.join(".hidden")).unwrap();
    // koreader sidecar — must not appear.
    fs::create_dir_all(root.join("notes.sdr")).unwrap();
    // Files.
    fs::write(root.join("one.pdf"), b"%PDF-stub\n").unwrap();
    fs::write(root.join("two.PDF"), b"%PDF-stub\n").unwrap(); // upper ext
    fs::write(root.join("README.md"), b"# noise").unwrap(); // unsupported
    fs::write(root.join(".config"), b"x").unwrap(); // hidden
    // Nested.
    fs::write(root.join("alpha/inner.pdf"), b"%PDF-stub\n").unwrap();
    root
}

#[test]
fn lists_only_supported_entries_and_hides_sidecars() {
    let root = make_fixture();
    let ex = ExplorerBuffer::open(BufferId(1), &root).unwrap();

    let names: Vec<String> = ex.entries.iter().map(|e| e.name.clone()).collect();
    // `..` always first when a parent exists.
    assert_eq!(names.first().map(String::as_str), Some(".."));
    // Dirs, sorted, before files.
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"beta".to_string()));
    // Hidden and sidecar are gone.
    assert!(!names.iter().any(|n| n == ".hidden"));
    assert!(!names.iter().any(|n| n == "notes.sdr"));
    assert!(!names.iter().any(|n| n == ".config"));
    // Unsupported file is gone.
    assert!(!names.iter().any(|n| n == "README.md"));
    // PDFs appear (case-insensitive extension match).
    assert!(names.contains(&"one.pdf".to_string()));
    assert!(names.contains(&"two.PDF".to_string()));

    // Kinds — ParentDir, then dirs, then pdfs.
    let parent_pos = ex
        .entries
        .iter()
        .position(|e| e.kind == ExplorerKind::ParentDir)
        .unwrap();
    let first_pdf_pos = ex
        .entries
        .iter()
        .position(|e| e.kind == ExplorerKind::Pdf)
        .unwrap();
    let last_dir_pos = ex
        .entries
        .iter()
        .rposition(|e| e.kind == ExplorerKind::Dir)
        .unwrap();
    assert!(parent_pos < last_dir_pos);
    assert!(last_dir_pos < first_pdf_pos);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn move_selection_clamps_at_edges() {
    let root = make_fixture();
    let mut ex = ExplorerBuffer::open(BufferId(1), &root).unwrap();
    let n = ex.entries.len();
    assert!(n >= 4);

    ex.move_selection(1);
    assert_eq!(ex.selected, 1);
    ex.move_selection(-100);
    assert_eq!(ex.selected, 0);
    ex.move_selection(1000);
    assert_eq!(ex.selected, n - 1);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn descend_into_dir_then_parent_returns_to_it() {
    let root = make_fixture();
    let mut ex = ExplorerBuffer::open(BufferId(1), &root).unwrap();

    // Position on "alpha".
    let alpha_idx = ex
        .entries
        .iter()
        .position(|e| e.name == "alpha")
        .expect("alpha present");
    ex.selected = alpha_idx;

    let alpha_path = ex.selected_path().expect("selected path");
    ex.set_cwd(alpha_path).unwrap();

    // We should now be inside alpha/ and see inner.pdf.
    assert!(ex.entries.iter().any(|e| e.name == "inner.pdf"));

    // Parent → back to root with `alpha` highlighted.
    ex.parent().unwrap();
    assert_eq!(
        ex.entries.get(ex.selected).map(|e| e.name.as_str()),
        Some("alpha")
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn selected_path_on_pdf_is_file_absolute() {
    let root = make_fixture();
    let mut ex = ExplorerBuffer::open(BufferId(1), &root).unwrap();
    let pdf_idx = ex
        .entries
        .iter()
        .position(|e| e.name == "one.pdf")
        .expect("one.pdf present");
    ex.selected = pdf_idx;

    let p = ex.selected_path().expect("selected path");
    assert!(p.ends_with("one.pdf"));
    assert!(p.is_absolute());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn parent_at_filesystem_root_is_noop() {
    // `/` has no parent. Walk far enough that `parent()` should
    // eventually stall, then call parent() once more — must not panic
    // and must not wipe entries.
    let mut cwd = std::env::current_dir().unwrap();
    while let Some(p) = cwd.parent() {
        cwd = p.to_path_buf();
    }
    let mut ex = ExplorerBuffer::open(BufferId(1), &cwd).unwrap();
    let before = ex.entries.len();
    ex.parent().unwrap();
    assert_eq!(ex.entries.len(), before);
}

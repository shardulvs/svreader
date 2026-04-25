use std::fs;

use svreader_core::{DocState, Rotation, ZoomMode};

#[test]
fn docstate_round_trip() {
    let tmp = tempdir_in(std::env::temp_dir(), "svreader-docstate-test");
    let pdf = tmp.join("book.pdf");
    // Make a dummy PDF file so sidecar_path() works.
    fs::write(&pdf, b"%PDF-1.4 fake").unwrap();

    let mut st = DocState::default();
    st.last_page = 42;
    st.zoom = ZoomMode::Custom(1.25);
    st.rotation = Rotation::R90;
    st.scroll_x = -10;
    st.scroll_y = 300;
    st.night_mode = true;
    st.render_dpi = Some(150.0);
    st.render_quality = 0.8;
    st.cache_enabled = false;
    st.save(&pdf).unwrap();

    let loaded = DocState::load(&pdf).unwrap();
    assert_eq!(loaded.last_page, 42);
    assert_eq!(loaded.rotation, Rotation::R90);
    assert_eq!(loaded.scroll_x, -10);
    assert_eq!(loaded.scroll_y, 300);
    assert!(loaded.night_mode);
    assert_eq!(loaded.render_dpi, Some(150.0));
    assert!((loaded.render_quality - 0.8).abs() < 1e-5);
    assert!(!loaded.cache_enabled);
    match loaded.zoom {
        ZoomMode::Custom(m) => assert!((m - 1.25).abs() < 1e-5),
        other => panic!("expected Custom, got {other:?}"),
    }

    fs::remove_dir_all(tmp).ok();
}

#[test]
fn docstate_preserves_unknown_keys() {
    let tmp = tempdir_in(std::env::temp_dir(), "svreader-docstate-unk");
    let pdf = tmp.join("book.pdf");
    fs::write(&pdf, b"%PDF-1.4 fake").unwrap();

    // Hand-write a metadata file with koreader-style extras.
    let sidecar = DocState::sidecar_path(&pdf);
    fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    let text = r#"-- test
return {
    ["last_page"] = 7,
    ["zoom"] = "fit-w",
    ["rotation"] = 0,
    ["scroll_x"] = 0,
    ["scroll_y"] = 0,
    ["night_mode"] = false,
    ["render_quality"] = 1.0,
    ["cache_enabled"] = true,
    ["bookmarks"] = {
        [1] = {
            ["notes"] = "koreader wrote this",
            ["page"] = 3,
        },
    },
    ["highlight_drawer"] = "lighten",
}
"#;
    fs::write(&sidecar, text).unwrap();

    let st = DocState::load(&pdf).unwrap();
    assert_eq!(st.last_page, 7);

    // Re-save — extras should still be in the output file.
    st.save(&pdf).unwrap();
    let out = fs::read_to_string(&sidecar).unwrap();
    assert!(out.contains("bookmarks"));
    assert!(out.contains("highlight_drawer"));
    assert!(out.contains("koreader wrote this"));

    fs::remove_dir_all(tmp).ok();
}

#[test]
fn docstate_bookmarks_round_trip() {
    let tmp = tempdir_in(std::env::temp_dir(), "svreader-docstate-marks");
    let pdf = tmp.join("book.pdf");
    fs::write(&pdf, b"%PDF-1.4 fake").unwrap();

    let mut st = DocState::default();
    st.set_bookmark('a', 5, 10, 20);
    st.set_bookmark('b', 100, 0, 1500);
    // Re-set 'a' to confirm it replaces rather than duplicates.
    st.set_bookmark('a', 7, -5, 30);
    st.mouse_enabled = Some(false);
    st.save(&pdf).unwrap();

    let loaded = DocState::load(&pdf).unwrap();
    assert_eq!(loaded.bookmarks.len(), 2);
    let a = loaded.find_bookmark('a').expect("a");
    assert_eq!(a.page, 7);
    assert_eq!(a.x_off, -5);
    assert_eq!(a.y_off, 30);
    let b = loaded.find_bookmark('b').expect("b");
    assert_eq!(b.page, 100);
    assert_eq!(b.y_off, 1500);
    assert_eq!(loaded.mouse_enabled, Some(false));

    fs::remove_dir_all(tmp).ok();
}

#[test]
fn docstate_delete_bookmark_clears_field_on_save() {
    let tmp = tempdir_in(std::env::temp_dir(), "svreader-docstate-delmark");
    let pdf = tmp.join("book.pdf");
    fs::write(&pdf, b"%PDF-1.4 fake").unwrap();

    let mut st = DocState::default();
    st.set_bookmark('a', 1, 0, 0);
    st.save(&pdf).unwrap();
    let mut st2 = DocState::load(&pdf).unwrap();
    assert!(st2.delete_bookmark('a'));
    st2.save(&pdf).unwrap();

    let st3 = DocState::load(&pdf).unwrap();
    assert!(st3.bookmarks.is_empty());
    fs::remove_dir_all(tmp).ok();
}

fn tempdir_in(parent: std::path::PathBuf, name: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let d = parent.join(format!("{}-{}-{}", name, pid, ts));
    fs::create_dir_all(&d).unwrap();
    d
}

//! Integration tests for the navigator state machine.
//!
//! Uses a `FakeDoc` — no mupdf required — so these run in a vanilla
//! `cargo test` environment.

use anyhow::Result;
use image::RgbaImage;
use svreader_core::commands::{CommandRegistry, ParsedCommand};
use svreader_core::document::{Document, PageSize};
use svreader_core::keys::{Key, KeyOutcome, KeyParser, KeyParserState};
use svreader_core::{Action, Navigator, Rotation, Viewport, ZoomMode};

struct FakeDoc {
    pages: Vec<PageSize>,
}

impl Document for FakeDoc {
    fn page_count(&self) -> usize {
        self.pages.len()
    }
    fn page_size(&self, i: usize) -> Result<PageSize> {
        Ok(self.pages[i])
    }
    fn render_page(&self, _i: usize, _scale: f32, _r: Rotation) -> Result<RgbaImage> {
        // Not exercised by navigator tests.
        Ok(RgbaImage::new(1, 1))
    }
}

fn doc_uniform(n: usize, w: f32, h: f32) -> FakeDoc {
    FakeDoc {
        pages: (0..n).map(|_| PageSize { width: w, height: h }).collect(),
    }
}

fn fit_width_viewport() -> Viewport {
    Viewport {
        screen_w: 600,
        screen_h: 400,
        zoom: ZoomMode::FitWidth,
        ..Viewport::default()
    }
}

#[test]
fn goto_clamps_into_range() {
    let doc = doc_uniform(5, 600.0, 800.0);
    let mut v = fit_width_viewport();
    Navigator::apply(&doc, &mut v, Action::GotoPage(99)).unwrap();
    assert_eq!(v.page_idx, 4);
    Navigator::apply(&doc, &mut v, Action::GotoPage(0)).unwrap();
    assert_eq!(v.page_idx, 0);
}

#[test]
fn next_screen_advances_then_flips_page() {
    // Tall page at fit-width → vertical scrolling room.
    let doc = doc_uniform(3, 600.0, 1600.0);
    let mut v = fit_width_viewport();
    Navigator::apply(&doc, &mut v, Action::SetZoom(ZoomMode::FitWidth)).unwrap();
    let before_idx = v.page_idx;
    let before_y = v.y_off;
    Navigator::apply(&doc, &mut v, Action::NextScreen).unwrap();
    assert_eq!(v.page_idx, before_idx);
    assert!(v.y_off > before_y);

    // Repeatedly advance until we cross a page boundary.
    for _ in 0..30 {
        Navigator::apply(&doc, &mut v, Action::NextScreen).unwrap();
        if v.page_idx > before_idx {
            break;
        }
    }
    assert!(v.page_idx > before_idx, "never crossed page boundary");
}

#[test]
fn next_screen_on_fitting_page_flips_page() {
    // Short page that fits entirely → j is just "next page".
    let doc = doc_uniform(3, 600.0, 300.0);
    let mut v = Viewport {
        screen_w: 600,
        screen_h: 400,
        zoom: ZoomMode::FitPage,
        ..Viewport::default()
    };
    Navigator::apply(&doc, &mut v, Action::SetZoom(ZoomMode::FitPage)).unwrap();
    assert!(v.page_fits(doc.page_size(0).unwrap()));
    Navigator::apply(&doc, &mut v, Action::NextScreen).unwrap();
    assert_eq!(v.page_idx, 1);
}

#[test]
fn centering_yields_negative_offsets_for_narrow_pages() {
    // A 600-wide screen, but the page is only 300 "units". Under
    // FitPage, page fits in both axes → offsets should sit centred.
    let doc = doc_uniform(1, 300.0, 300.0);
    let mut v = Viewport {
        screen_w: 600,
        screen_h: 400,
        zoom: ZoomMode::FitPage,
        ..Viewport::default()
    };
    Navigator::apply(&doc, &mut v, Action::SetZoom(ZoomMode::FitPage)).unwrap();
    let size = doc.page_size(0).unwrap();
    let (pw, ph) = v.composed_page_size(size);
    let (xmin, xmax) = v.x_range(pw);
    let (ymin, ymax) = v.y_range(ph);
    assert_eq!(xmin, xmax);
    assert_eq!(ymin, ymax);
    assert!(xmin <= 0);
    assert!(ymin <= 0);
}

#[test]
fn rotation_cycles() {
    let doc = doc_uniform(1, 600.0, 800.0);
    let mut v = fit_width_viewport();
    assert_eq!(v.rotation, Rotation::R0);
    Navigator::apply(&doc, &mut v, Action::RotateCw).unwrap();
    assert_eq!(v.rotation, Rotation::R90);
    Navigator::apply(&doc, &mut v, Action::RotateCw).unwrap();
    assert_eq!(v.rotation, Rotation::R180);
    Navigator::apply(&doc, &mut v, Action::RotateCcw).unwrap();
    assert_eq!(v.rotation, Rotation::R90);
    Navigator::apply(&doc, &mut v, Action::SetRotation(Rotation::R270)).unwrap();
    assert_eq!(v.rotation, Rotation::R270);
}

#[test]
fn night_toggle_round_trip() {
    let doc = doc_uniform(1, 600.0, 800.0);
    let mut v = fit_width_viewport();
    assert!(!v.night_mode);
    Navigator::apply(&doc, &mut v, Action::ToggleNight).unwrap();
    assert!(v.night_mode);
    Navigator::apply(&doc, &mut v, Action::SetNight(false)).unwrap();
    assert!(!v.night_mode);
}

#[test]
fn key_parser_counts_and_leader() {
    let mut st = KeyParserState::default();
    assert_eq!(KeyParser::feed(&mut st, Key::Char('4')), KeyOutcome::Pending);
    assert_eq!(KeyParser::feed(&mut st, Key::Char('2')), KeyOutcome::Pending);
    let out = KeyParser::feed(&mut st, Key::Char('j'));
    match out {
        KeyOutcome::Action { action, count } => {
            assert_eq!(action, Action::NextScreen);
            assert_eq!(count, 42);
        }
        _ => panic!("want action"),
    }
    assert!(!st.active());
}

#[test]
fn key_parser_gg_jumps_to_first() {
    let mut st = KeyParserState::default();
    assert_eq!(KeyParser::feed(&mut st, Key::Char('g')), KeyOutcome::Pending);
    let out = KeyParser::feed(&mut st, Key::Char('g'));
    assert_eq!(out, KeyOutcome::Action { action: Action::GotoPage(0), count: 1 });
}

#[test]
fn key_parser_count_gg_goes_to_that_page() {
    let mut st = KeyParserState::default();
    for c in "7g".chars() {
        KeyParser::feed(&mut st, Key::Char(c));
    }
    let out = KeyParser::feed(&mut st, Key::Char('g'));
    assert_eq!(out, KeyOutcome::Action { action: Action::GotoPage(6), count: 1 });
}

#[test]
fn key_parser_capital_G_last_page_or_goto() {
    let mut st = KeyParserState::default();
    let out = KeyParser::feed(&mut st, Key::Char('G'));
    assert_eq!(out, KeyOutcome::Action { action: Action::LastPage, count: 1 });
    let mut st = KeyParserState::default();
    for c in "42".chars() {
        KeyParser::feed(&mut st, Key::Char(c));
    }
    let out = KeyParser::feed(&mut st, Key::Char('G'));
    assert_eq!(out, KeyOutcome::Action { action: Action::GotoPage(41), count: 1 });
}

#[test]
fn key_parser_ctrl_chords() {
    let mut st = KeyParserState::default();
    let out = KeyParser::feed(&mut st, Key::Ctrl('d'));
    assert_eq!(
        out,
        KeyOutcome::Action {
            action: Action::HalfScreenDown,
            count: 1
        }
    );
    let out = KeyParser::feed(&mut st, Key::Ctrl('f'));
    assert_eq!(
        out,
        KeyOutcome::Action {
            action: Action::NextPage,
            count: 1
        }
    );
}

#[test]
fn key_parser_esc_cancels_count() {
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('5'));
    assert!(st.active());
    KeyParser::feed(&mut st, Key::Esc);
    assert!(!st.active());
}

#[test]
fn command_registry_parses_all_commands() {
    let r = CommandRegistry::default();
    // Every registered command must parse with at least one arg example.
    for cmd in r.commands() {
        let arg = match cmd.name {
            "goto" => "5",
            "zoom" => "fit-w",
            "rotate" => "90",
            "night" => "toggle",
            "dpi" => "auto",
            "quality" => "100",
            "cache" => "on",
            "cache-size" => "8",
            "prefetch" => "2",
            "colors" => "xterm256",
            _ => "",
        };
        let line = if arg.is_empty() {
            cmd.name.to_string()
        } else {
            format!("{} {}", cmd.name, arg)
        };
        let parsed = r
            .parse(&line)
            .unwrap_or_else(|e| panic!(":{} ({}): {}", cmd.name, line, e));
        let _ = parsed;
    }
}

#[test]
fn command_registry_completes_prefix() {
    let r = CommandRegistry::default();
    let hits: Vec<_> = r.complete("ca").into_iter().map(|c| c.name).collect();
    assert!(hits.contains(&"cache"));
    assert!(hits.contains(&"cache-size"));
}

#[test]
fn command_registry_parses_quit_and_help() {
    let r = CommandRegistry::default();
    assert_eq!(r.parse("quit").unwrap(), ParsedCommand::Quit);
    assert_eq!(r.parse("q").unwrap(), ParsedCommand::Quit);
    assert_eq!(r.parse("help").unwrap(), ParsedCommand::Help);
}

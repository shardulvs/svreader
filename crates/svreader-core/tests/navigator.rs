//! Integration tests for the navigator state machine.
//!
//! Uses a `FakeDoc` — no mupdf required — so these run in a vanilla
//! `cargo test` environment.

use anyhow::Result;
use image::RgbaImage;
use std::path::PathBuf;

use svreader_core::commands::{CommandRegistry, ParsedCommand, SplitDirection};
use svreader_core::document::{Document, PageMetrics, PageSize};
use svreader_core::keys::{
    ArrowDir, Key, KeyOutcome, KeyParser, KeyParserState, Leader, PageDir, WindowOp,
};
use svreader_core::{Action, Navigator, Rotation, Viewport, ZoomMode};

struct FakeDoc {
    pages: Vec<PageSize>,
}

impl PageMetrics for FakeDoc {
    fn page_count(&self) -> usize {
        self.pages.len()
    }
    fn page_size(&self, i: usize) -> Result<PageSize> {
        Ok(self.pages[i])
    }
}

impl Document for FakeDoc {
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
            "ecache" => "on",
            "ecache-size" => "20",
            "prefetch" => "2",
            "colors" => "xterm256",
            "edit" | "open" => "foo.pdf",
            "buffer" => "1",
            "tabmove" => "+1",
            "resize" | "vresize" => "+2",
            "delmark" => "a",
            "mouse" => "on",
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
fn command_registry_parses_window_and_quit() {
    let r = CommandRegistry::default();
    // Vim semantics: :q / :quit close the current window; :qa / :qall
    // hard-quit the app.
    assert_eq!(r.parse("q").unwrap(), ParsedCommand::CloseWindow);
    assert_eq!(r.parse("quit").unwrap(), ParsedCommand::CloseWindow);
    assert_eq!(r.parse("close").unwrap(), ParsedCommand::CloseWindow);
    assert_eq!(r.parse("qa").unwrap(), ParsedCommand::Quit);
    assert_eq!(r.parse("qall").unwrap(), ParsedCommand::Quit);
    assert_eq!(r.parse("help").unwrap(), ParsedCommand::Help);
}

#[test]
fn command_registry_parses_splits_and_tabs() {
    let r = CommandRegistry::default();
    assert_eq!(
        r.parse("vsplit").unwrap(),
        ParsedCommand::Split {
            direction: SplitDirection::Vertical,
            file: None,
        }
    );
    assert_eq!(
        r.parse("vsp foo.pdf").unwrap(),
        ParsedCommand::Split {
            direction: SplitDirection::Vertical,
            file: Some(PathBuf::from("foo.pdf")),
        }
    );
    assert_eq!(
        r.parse("split bar.pdf").unwrap(),
        ParsedCommand::Split {
            direction: SplitDirection::Horizontal,
            file: Some(PathBuf::from("bar.pdf")),
        }
    );
    assert_eq!(
        r.parse("tabnew baz.pdf").unwrap(),
        ParsedCommand::TabNew(Some(PathBuf::from("baz.pdf")))
    );
    assert_eq!(r.parse("tabnew").unwrap(), ParsedCommand::TabNew(None));
    assert_eq!(r.parse("tabclose").unwrap(), ParsedCommand::TabClose);
    assert_eq!(r.parse("tabonly").unwrap(), ParsedCommand::TabOnly);
    assert_eq!(r.parse("only").unwrap(), ParsedCommand::OnlyWindow);
    assert_eq!(
        r.parse("edit foo.pdf").unwrap(),
        ParsedCommand::Edit(PathBuf::from("foo.pdf"))
    );
    assert_eq!(
        r.parse("open bar.pdf").unwrap(),
        ParsedCommand::Edit(PathBuf::from("bar.pdf"))
    );
    // :e and :o short forms parse the same way.
    assert_eq!(
        r.parse("e quux.pdf").unwrap(),
        ParsedCommand::Edit(PathBuf::from("quux.pdf"))
    );
    assert_eq!(
        r.parse("o quux.pdf").unwrap(),
        ParsedCommand::Edit(PathBuf::from("quux.pdf"))
    );
    // `:b 3` = :buffer 3
    assert_eq!(r.parse("b 3").unwrap(), ParsedCommand::Buffer(3));
}

#[test]
fn key_parser_ctrl_w_chord() {
    let mut st = KeyParserState::default();
    assert_eq!(KeyParser::feed(&mut st, Key::Ctrl('w')), KeyOutcome::Pending);
    assert_eq!(st.leader, Leader::CtrlW);
    let out = KeyParser::feed(&mut st, Key::Char('l'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::FocusRight));
    assert_eq!(st.leader, Leader::None);
    // <C-w><C-j> — ctrl-prefixed variants are accepted too.
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Ctrl('w'));
    let out = KeyParser::feed(&mut st, Key::Ctrl('j'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::FocusDown));
}

#[test]
fn key_parser_ctrl_w_split_and_close() {
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Ctrl('w'));
    assert_eq!(
        KeyParser::feed(&mut st, Key::Char('s')),
        KeyOutcome::Window(WindowOp::SplitHorizontal)
    );
    KeyParser::feed(&mut st, Key::Ctrl('w'));
    assert_eq!(
        KeyParser::feed(&mut st, Key::Char('v')),
        KeyOutcome::Window(WindowOp::SplitVertical)
    );
    KeyParser::feed(&mut st, Key::Ctrl('w'));
    assert_eq!(
        KeyParser::feed(&mut st, Key::Char('c')),
        KeyOutcome::Window(WindowOp::Close)
    );
    KeyParser::feed(&mut st, Key::Ctrl('w'));
    assert_eq!(
        KeyParser::feed(&mut st, Key::Char('o')),
        KeyOutcome::Window(WindowOp::Only)
    );
}

#[test]
fn key_parser_count_with_ctrl_w_resize() {
    // `3<C-w>+` → grow current window by 3.
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('3'));
    KeyParser::feed(&mut st, Key::Ctrl('w'));
    let out = KeyParser::feed(&mut st, Key::Char('+'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::ResizeVertical(3)));
}

#[test]
fn key_parser_gt_switches_tabs() {
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('g'));
    let out = KeyParser::feed(&mut st, Key::Char('t'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::NextTab(1)));
    // 2gt
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('2'));
    KeyParser::feed(&mut st, Key::Char('g'));
    let out = KeyParser::feed(&mut st, Key::Char('t'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::NextTab(2)));
    // gT
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('g'));
    let out = KeyParser::feed(&mut st, Key::Char('T'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::PrevTab(1)));
}

#[test]
fn key_parser_alternate_buffer() {
    let mut st = KeyParserState::default();
    let out = KeyParser::feed(&mut st, Key::Ctrl('^'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::AlternateBuffer));
    let mut st = KeyParserState::default();
    let out = KeyParser::feed(&mut st, Key::Ctrl('6'));
    assert_eq!(out, KeyOutcome::Window(WindowOp::AlternateBuffer));
}

#[test]
fn key_parser_alt_arrow_focus_moves() {
    let mut st = KeyParserState::default();
    assert_eq!(
        KeyParser::feed(&mut st, Key::AltArrow(ArrowDir::Left)),
        KeyOutcome::Window(WindowOp::FocusLeft)
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::AltArrow(ArrowDir::Right)),
        KeyOutcome::Window(WindowOp::FocusRight)
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::AltArrow(ArrowDir::Up)),
        KeyOutcome::Window(WindowOp::FocusUp)
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::AltArrow(ArrowDir::Down)),
        KeyOutcome::Window(WindowOp::FocusDown)
    );
}

#[test]
fn key_parser_shift_alt_arrow_resizes() {
    let mut st = KeyParserState::default();
    // Default magnitude is 2.
    assert_eq!(
        KeyParser::feed(&mut st, Key::ShiftAltArrow(ArrowDir::Left)),
        KeyOutcome::Window(WindowOp::ResizeHorizontal(-2))
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::ShiftAltArrow(ArrowDir::Right)),
        KeyOutcome::Window(WindowOp::ResizeHorizontal(2))
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::ShiftAltArrow(ArrowDir::Up)),
        KeyOutcome::Window(WindowOp::ResizeVertical(-2))
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::ShiftAltArrow(ArrowDir::Down)),
        KeyOutcome::Window(WindowOp::ResizeVertical(2))
    );
    // Count prefix overrides the default 2.
    KeyParser::feed(&mut st, Key::Char('5'));
    assert_eq!(
        KeyParser::feed(&mut st, Key::ShiftAltArrow(ArrowDir::Right)),
        KeyOutcome::Window(WindowOp::ResizeHorizontal(5))
    );
}

#[test]
fn key_parser_ctrl_page_switches_tabs() {
    let mut st = KeyParserState::default();
    assert_eq!(
        KeyParser::feed(&mut st, Key::CtrlPage(PageDir::Up)),
        KeyOutcome::Window(WindowOp::PrevTab(1))
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::CtrlPage(PageDir::Down)),
        KeyOutcome::Window(WindowOp::NextTab(1))
    );
}

#[test]
fn key_parser_ctrl_shift_page_moves_tab() {
    let mut st = KeyParserState::default();
    assert_eq!(
        KeyParser::feed(&mut st, Key::CtrlShiftPage(PageDir::Up)),
        KeyOutcome::Window(WindowOp::MoveTabLeft)
    );
    assert_eq!(
        KeyParser::feed(&mut st, Key::CtrlShiftPage(PageDir::Down)),
        KeyOutcome::Window(WindowOp::MoveTabRight)
    );
}

#[test]
fn command_registry_parses_resize_and_tabmove() {
    let r = CommandRegistry::default();
    assert_eq!(r.parse("resize +3").unwrap(), ParsedCommand::Resize(3));
    assert_eq!(r.parse("resize -3").unwrap(), ParsedCommand::Resize(-3));
    assert_eq!(r.parse("resize 5").unwrap(), ParsedCommand::Resize(5));
    assert_eq!(r.parse("vresize -2").unwrap(), ParsedCommand::VResize(-2));
    // Vim's `:vertical resize N` prefix form.
    assert_eq!(
        r.parse("vertical resize +4").unwrap(),
        ParsedCommand::VResize(4)
    );
    assert_eq!(r.parse("tabmove +1").unwrap(), ParsedCommand::TabMove(1));
    assert_eq!(r.parse("tabmove -1").unwrap(), ParsedCommand::TabMove(-1));
}

// ============================ M2 tests ============================

#[test]
fn key_parser_set_mark_letter() {
    let mut st = KeyParserState::default();
    let r = KeyParser::feed(&mut st, Key::Char('m'));
    assert_eq!(r, KeyOutcome::Pending);
    assert_eq!(st.leader, Leader::M);
    let r = KeyParser::feed(&mut st, Key::Char('a'));
    assert_eq!(r, KeyOutcome::SetMark('a'));
    assert!(!st.active());
}

#[test]
fn key_parser_set_mark_non_letter_aborts_cleanly() {
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('m'));
    let r = KeyParser::feed(&mut st, Key::Char('1'));
    assert_eq!(r, KeyOutcome::Pending);
    assert!(!st.active());
}

#[test]
fn key_parser_jump_mark_apostrophe_and_backtick() {
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('\''));
    let r = KeyParser::feed(&mut st, Key::Char('Z'));
    assert_eq!(r, KeyOutcome::JumpMark('Z'));
    let mut st = KeyParserState::default();
    KeyParser::feed(&mut st, Key::Char('`'));
    let r = KeyParser::feed(&mut st, Key::Char('z'));
    assert_eq!(r, KeyOutcome::JumpMark('z'));
}

#[test]
fn key_parser_ctrl_o_jumps_back() {
    let mut st = KeyParserState::default();
    let r = KeyParser::feed(&mut st, Key::Ctrl('o'));
    assert_eq!(r, KeyOutcome::JumpBack);
}

#[test]
fn key_parser_t_toggles_toc() {
    let mut st = KeyParserState::default();
    let r = KeyParser::feed(&mut st, Key::Char('t'));
    assert_eq!(r, KeyOutcome::ToggleToc);
}

#[test]
fn navigator_jump_to_clamps_offsets() {
    let doc = doc_uniform(3, 600.0, 800.0);
    let mut vp = Viewport {
        screen_w: 600,
        screen_h: 400,
        zoom: ZoomMode::FitWidth,
        ..Default::default()
    };
    Navigator::apply(
        &doc,
        &mut vp,
        Action::JumpTo {
            page_idx: 1,
            x_off: 9999,
            y_off: 9999,
        },
    )
    .unwrap();
    assert_eq!(vp.page_idx, 1);
    // y_off clamped into the valid scroll range; not the wild
    // out-of-bounds value we asked for.
    let (_, ymax) = vp.y_range(vp.composed_page_size(doc.pages[1]).1);
    assert_eq!(vp.y_off, ymax);
}

#[test]
fn navigator_jump_to_out_of_range_clamps_page() {
    let doc = doc_uniform(3, 600.0, 800.0);
    let mut vp = Viewport::default();
    Navigator::apply(
        &doc,
        &mut vp,
        Action::JumpTo {
            page_idx: 99,
            x_off: 0,
            y_off: 0,
        },
    )
    .unwrap();
    assert_eq!(vp.page_idx, 2);
}

#[test]
fn command_registry_parses_m2_commands() {
    let r = CommandRegistry::default();
    assert_eq!(r.parse("toc").unwrap(), ParsedCommand::ToggleToc);
    assert_eq!(r.parse("marks").unwrap(), ParsedCommand::ToggleMarks);
    assert_eq!(r.parse("bookmarks").unwrap(), ParsedCommand::ToggleMarks);
    assert_eq!(r.parse("delmark q").unwrap(), ParsedCommand::DeleteMark('q'));
    assert_eq!(r.parse("back").unwrap(), ParsedCommand::JumpBack);
    assert_eq!(r.parse("forward").unwrap(), ParsedCommand::JumpForward);
    assert_eq!(r.parse("mouse on").unwrap(), ParsedCommand::MouseSet(true));
    assert_eq!(r.parse("mouse off").unwrap(), ParsedCommand::MouseSet(false));
    assert_eq!(r.parse("mouse").unwrap(), ParsedCommand::MouseToggle);
}

#[test]
fn command_delmark_rejects_non_letter() {
    let r = CommandRegistry::default();
    assert!(r.parse("delmark 1").is_err());
    assert!(r.parse("delmark").is_err());
}

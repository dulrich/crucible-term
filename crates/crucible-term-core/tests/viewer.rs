//! The viewer frame encoder: round trip, scroll cost, idle, modes and baseline.
mod common;

use common::dense_fixture::dense_lines;
use common::{Rng, edit_corpus, fed, text_corpus};
use crucible_term_core::{
    Cell, CursorKind, CursorStyle, Grid, MouseEncoding, MouseMode, Session, TermModes, Viewer,
    Width,
};

/// What the viewer must show: the source's screen clipped to `cols x rows` (wide glyphs cut by
/// the right edge blank, outside the grid blank).
fn clipped(g: &Grid, cols: usize, rows: usize) -> Vec<Vec<Cell>> {
    (0..rows)
        .map(|y| {
            (0..cols)
                .map(|x| match g.screen.get(y).and_then(|r| r.cells.get(x)) {
                    Some(c) if !(c.width == Width::Wide && x + 1 == cols) => c.clone(),
                    _ => Cell::default(),
                })
                .collect()
        })
        .collect()
}

fn cells(s: &Session) -> Vec<Vec<Cell>> {
    s.screen().screen.into_iter().map(|r| r.cells).collect()
}

/// Encode a frame from `src`, feed it to `oracle`, commit.
fn step(v: &mut Viewer, src: &Session, oracle: &mut Session) -> usize {
    let f = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    oracle.feed(f.bytes());
    let n = f.bytes().len();
    v.commit(f);
    n
}

fn check(src: &Session, oracle: &Session, cols: usize, rows: usize, what: &str) {
    let g = src.screen();
    let want = clipped(&g, cols, rows);
    let got = cells(oracle);
    if let Some(y) = (0..rows).find(|&y| want[y] != got[y]) {
        panic!(
            "{what}: row {y} differs\nwant {:?}\n got {:?}",
            want[y], got[y]
        );
    }
    let (cx, cy) = g.cursor;
    if cx < cols.min(g.cols) && cy < rows.min(g.rows) {
        assert_eq!(oracle.cursor(), g.cursor, "{what}: cursor");
    }
}

/// Round trip for viewers smaller than, equal to and larger than the session, over edits,
/// scrolling output, cut wide glyphs, combining marks and skipped frames.
#[test]
fn round_trip_any_size_with_skipped_frames() {
    let (sc, sr) = (60, 20);
    for &(vc, vr) in &[(41, 9), (60, 20), (83, 27), (60, 12), (30, 20)] {
        let mut src = Session::new(sc, sr, 0);
        let mut v = Viewer::new(vc, vr);
        let mut oracle = Session::new(vc, vr, 0);
        oracle.feed(&Viewer::enter());
        let edits = edit_corpus(7 + vc as u64, 30_000);
        let text = text_corpus(200);
        let mut r = Rng(0x1234 + vr as u64);
        let (mut ei, mut ti) = (0, 0);
        for n in 0..120 {
            match r.below(4) {
                0 | 1 => {
                    let len = 1 + r.below(400) as usize;
                    let end = (ei + len).min(edits.len());
                    src.feed(&edits[ei..end]);
                    ei = end;
                }
                2 => {
                    let len = 1 + r.below(600) as usize;
                    let end = (ti + len).min(text.len());
                    src.feed(&text[ti..end]);
                    ti = end;
                }
                _ => {
                    // A wide glyph on the viewer's right edge and a combining mark.
                    src.feed(format!("\x1b[{};{}H漢字e\u{301}", 1 + n % sr, vc.min(sc)).as_bytes());
                }
            }
            // Skip frames: only every third update is encoded and committed.
            if n % 3 == 0 {
                step(&mut v, &src, &mut oracle);
                check(&src, &oracle, vc, vr, &format!("{vc}x{vr} frame {n}"));
            }
        }
    }
}

/// A frame encoded from an older baseline (several source updates, frames dropped uncommitted)
/// still lands exactly; an uncommitted frame does not move the baseline.
#[test]
fn uncommitted_frame_leaves_the_baseline() {
    let mut src = fed(40, 10, 0, b"hello\r\nworld");
    let mut v = Viewer::new(40, 10);
    let mut oracle = Session::new(40, 10, 0);
    oracle.feed(&Viewer::enter());
    step(&mut v, &src, &mut oracle);
    src.feed(b"\r\nmore");
    let dropped = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    assert!(!dropped.is_empty());
    src.feed(b"\r\nagain");
    let a = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    let b = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    assert_eq!(
        a.bytes(),
        b.bytes(),
        "frame depends only on the committed state"
    );
    oracle.feed(a.bytes());
    v.commit(a);
    check(&src, &oracle, 40, 10, "after dropped frame");
}

/// An unchanged screen and modes encode to zero bytes.
#[test]
fn idle_is_zero_bytes() {
    let src = fed(80, 24, 0, b"\x1b[?2004h\x1b[?1002h\x1b[5 q$ ls\r\n");
    let mut v = Viewer::new(80, 24);
    let f = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    assert!(!f.is_empty());
    v.commit(f);
    let f = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    assert!(f.is_empty(), "{:?}", String::from_utf8_lossy(f.bytes()));
}

/// Resizing the viewer or the session repaints, and the repaint is exact.
#[test]
fn resizes_repaint() {
    let mut src = fed(50, 10, 0, &text_corpus(30));
    let mut v = Viewer::new(50, 10);
    let mut oracle = Session::new(50, 10, 0);
    oracle.feed(&Viewer::enter());
    step(&mut v, &src, &mut oracle);
    v.resize(30, 8);
    oracle.resize(30, 8);
    let n = step(&mut v, &src, &mut oracle);
    assert!(n > 0);
    check(&src, &oracle, 30, 8, "viewer resize");
    src.resize(20, 6);
    step(&mut v, &src, &mut oracle);
    check(&src, &oracle, 30, 8, "session resize");
}

fn filled_dense(cols: usize, rows: usize) -> (Session, Rng) {
    let mut r = Rng(0xdead_beef);
    let s = fed(cols, rows, 0, &dense_lines(&mut r, cols, rows + 5));
    (s, r)
}

fn scroll_cost(k: usize) -> usize {
    let (mut src, mut r) = filled_dense(200, 50);
    let mut v = Viewer::new(200, 50);
    let mut oracle = Session::new(200, 50, 0);
    oracle.feed(&Viewer::enter());
    step(&mut v, &src, &mut oracle);
    src.feed(&dense_lines(&mut r, 200, k));
    let n = step(&mut v, &src, &mut oracle);
    check(&src, &oracle, 200, 50, "dense scroll");
    n
}

/// One new dense line at 200x50 costs at most 2 KB (the cell delta needs 24 KB).
#[test]
fn one_line_scroll_is_small() {
    let n = scroll_cost(1);
    eprintln!("one dense line: {n} B");
    assert!(n <= 2048, "{n} bytes");
}

/// A k-row scroll costs in proportion to k.
#[test]
fn k_row_scroll_is_proportional() {
    let one = scroll_cost(1);
    let five = scroll_cost(5);
    eprintln!("five dense lines: {five} B");
    assert!(five <= 6 * one + 64, "1: {one} B, 5: {five} B");
    assert!(five <= 5 * 2048);
}

/// Scrolling with a viewer taller than the session keeps the blank area below the grid.
#[test]
fn scroll_in_larger_viewer_keeps_margin_blank() {
    let (mut src, mut r) = filled_dense(60, 10);
    let mut v = Viewer::new(80, 16);
    let mut oracle = Session::new(80, 16, 0);
    oracle.feed(&Viewer::enter());
    step(&mut v, &src, &mut oracle);
    src.feed(&dense_lines(&mut r, 60, 2));
    let n = step(&mut v, &src, &mut oracle);
    check(&src, &oracle, 80, 16, "taller viewer scroll");
    assert!(n < 1200, "{n} bytes");
}

/// 20 cells changed in place stay small (the cell delta needs 37 B).
#[test]
fn in_place_change_is_small() {
    let (mut src, _) = filled_dense(200, 50);
    let mut v = Viewer::new(200, 50);
    let mut oracle = Session::new(200, 50, 0);
    oracle.feed(&Viewer::enter());
    step(&mut v, &src, &mut oracle);
    src.feed(b"\x1b[20;30H01234567890123456789");
    let n = step(&mut v, &src, &mut oracle);
    check(&src, &oracle, 200, 50, "in place");
    eprintln!("20 cells in place: {n} B");
    assert!(n <= 100, "{n} bytes");
}

fn modes_frame(v: &mut Viewer, modes: TermModes, style: CursorStyle) -> String {
    let g = Session::new(10, 3, 0).screen();
    let f = v.frame(&g, &modes, style);
    let s = String::from_utf8(f.bytes().to_vec()).expect("utf8");
    v.commit(f);
    s
}

/// Mode transitions are emitted byte for byte, only when they change.
#[test]
fn mode_transitions_byte_for_byte() {
    let mut v = Viewer::new(10, 3);
    let block = CursorStyle::default();
    // First frame: repaint, cursor home, block shape (DECSCUSR 2).
    assert_eq!(
        modes_frame(&mut v, TermModes::default(), block),
        "\x1b[0m\x1b[H\x1b[2J\x1b[1;1H\x1b[2 q"
    );
    let m = TermModes {
        app_cursor: true,
        app_keypad: true,
        kitty_flags: 0b101,
        mouse: MouseMode::Drag,
        mouse_encoding: MouseEncoding::Sgr,
        bracketed_paste: true,
        focus_events: true,
        alt_screen: true,
        alternate_scroll: true,
    };
    let beam = CursorStyle {
        shape: CursorKind::Beam,
        blinking: true,
    };
    assert_eq!(
        modes_frame(&mut v, m, beam),
        "\x1b[5 q\x1b[?1h\x1b=\x1b[?2004h\x1b[?1004h\x1b[?1002h\x1b[?1006h\x1b[=5;1u"
    );
    assert_eq!(modes_frame(&mut v, m, beam), "");
    // Mouse change resets the old mode first; hidden cursor keeps the shape.
    let m2 = TermModes {
        mouse: MouseMode::Any,
        mouse_encoding: MouseEncoding::Default,
        ..m
    };
    let hidden = CursorStyle {
        shape: CursorKind::Hidden,
        blinking: true,
    };
    assert_eq!(
        modes_frame(&mut v, m2, hidden),
        "\x1b[?25l\x1b[?1002l\x1b[?1003h\x1b[?1006l"
    );
    let m3 = TermModes {
        mouse: MouseMode::Click,
        ..m2
    };
    assert_eq!(
        modes_frame(
            &mut v,
            m3,
            CursorStyle {
                shape: CursorKind::Underline,
                blinking: false
            }
        ),
        "\x1b[4 q\x1b[?25h\x1b[?1003l\x1b[?1000h"
    );
    // alt_screen and alternate_scroll are not mirrored.
    let m4 = TermModes {
        alt_screen: false,
        alternate_scroll: false,
        ..m3
    };
    let ul = CursorStyle {
        shape: CursorKind::Underline,
        blinking: false,
    };
    assert_eq!(modes_frame(&mut v, m4, ul), "");
    // Leave resets exactly what was set, then pops the kitty entry.
    assert_eq!(
        String::from_utf8(v.leave()).expect("utf8"),
        "\x1b[0m\x1b[r\x1b[0 q\x1b[?1l\x1b>\x1b[?2004l\x1b[?1004l\x1b[?1000l\x1b[?25h\x1b[<u\x1b[?1049l"
    );
}

/// Enter, and leave with no frame ever delivered.
#[test]
fn enter_and_bare_leave() {
    assert_eq!(Viewer::enter(), b"\x1b[?1049h\x1b[>0u\x1b[0m\x1b[H\x1b[2J");
    assert_eq!(
        Viewer::new(80, 24).leave(),
        b"\x1b[0m\x1b[r\x1b[?25h\x1b[<u\x1b[?1049l"
    );
}

/// The cursor is hidden while it lies outside the clip, and shown again inside.
#[test]
fn cursor_outside_clip_is_hidden() {
    let mut src = fed(40, 10, 0, b"\x1b[8;30Hx");
    let mut v = Viewer::new(20, 5);
    let f = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    let s = String::from_utf8_lossy(f.bytes()).into_owned();
    assert!(s.ends_with("\x1b[2 q\x1b[?25l"), "{s:?}");
    v.commit(f);
    src.feed(b"\x1b[2;3H");
    let f = v.frame(&src.screen(), &src.modes(), src.cursor_style());
    assert_eq!(f.bytes(), b"\x1b[2;3H\x1b[?25h");
}

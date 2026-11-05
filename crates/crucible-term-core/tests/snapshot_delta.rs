//! Generic VT snapshot, cell-level delta and reflow.
mod common;

use common::{edit_corpus, fed, text_corpus};
use crucible_term_core::{Attrs, Color, Delta, Grid, PromptMark, Session, Underline, Width};

fn restore(snapshot: &[u8], cols: usize, rows: usize, sb: usize) -> Session {
    fed(cols, rows, sb, snapshot)
}

/// The spike's round trip: a 200x50 session with 10k rows of scrollback, snapshotted as VT and
/// fed to a fresh session, reproduces every row and the cursor exactly.
#[test]
fn snapshot_round_trips_10k_rows_exactly() {
    let mut s = fed(200, 50, 10_000, &text_corpus(10_050));
    s.feed(b"\x1b[38;2;10;200;30mgreen \x1b[1mbold\x1b[0m \x1b[4:3mcurly\x1b[0m\r\n");
    let g = s.grid();
    assert_eq!(g.history.len(), 10_000, "scrollback filled");
    assert!(g.history.iter().any(|r| r.wrapped), "corpus soft-wraps");
    let snap = s.snapshot();
    let r = restore(&snap, 200, 50, 10_000);
    let rg = r.grid();
    let first = rg.history.iter().zip(&g.history).position(|(a, b)| a != b);
    assert_eq!(first, None, "first differing history row");
    assert_eq!(rg, g);
    assert!(snap.len() < 2_000_000, "snapshot size {}", snap.len());
}

/// E1: exactly the documented attribute set survives a snapshot.
#[test]
fn snapshot_fidelity_boundary_is_the_documented_set() {
    // Destructuring pins the carried field set: adding an attribute breaks this test until the
    // fidelity docs and encoders are extended.
    let Attrs {
        fg: _,
        bg: _,
        bold: _,
        italic: _,
        faint: _,
        inverse: _,
        strikethrough: _,
        underline: _,
    } = Attrs::default();
    let input = concat!(
        "\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[2mF\x1b[0m\x1b[7mV\x1b[0m\x1b[9mS\x1b[0m",
        "\x1b[4mu\x1b[4:2mu\x1b[4:3mu\x1b[4:4mu\x1b[4:5mu\x1b[0m",
        "\x1b[31mr\x1b[94mb\x1b[38;5;200mi\x1b[38;2;1;2;3mt\x1b[0m",
        "\x1b[41m \x1b[48;5;17m \x1b[48;2;9;8;7m \x1b[0m",
        "漢e\u{301}\r\n",
        "\x1b]133;A\x07$ \x1b]8;;https://x/\x1b\\L\x1b]8;;\x1b\\\x1b[?7l\x1b[3;5H",
    );
    let s = fed(80, 24, 100, input.as_bytes());
    let g = s.grid();
    let r = restore(&s.snapshot(), 80, 24, 100);
    assert_eq!(r.grid(), g);
    let row0 = &g.screen[0].cells;
    let a = |i: usize| row0[i].attrs;
    assert!(a(0).bold && a(1).italic && a(2).faint && a(3).inverse && a(4).strikethrough);
    let uls: Vec<Underline> = (5..10).map(|i| a(i).underline).collect();
    assert_eq!(
        uls,
        [
            Underline::Single,
            Underline::Double,
            Underline::Curly,
            Underline::Dotted,
            Underline::Dashed
        ]
    );
    assert_eq!(
        (a(10).fg, a(11).fg, a(12).fg, a(13).fg),
        (
            Color::Indexed(1),
            Color::Indexed(12),
            Color::Indexed(200),
            Color::Rgb(1, 2, 3)
        )
    );
    assert_eq!(
        (a(14).bg, a(15).bg, a(16).bg),
        (Color::Indexed(1), Color::Indexed(17), Color::Rgb(9, 8, 7))
    );
    assert_eq!(
        (row0[17].text.as_str(), row0[17].width, row0[18].width),
        ("漢", Width::Wide, Width::Spacer)
    );
    assert_eq!(row0[19].text, "e\u{301}");
    assert_eq!(g.screen[1].prompt, PromptMark::Prompt);
    assert_eq!(g.cursor, (4, 2));
    // Not carried: DECAWM off. The restored session still autowraps.
    let mut orig = s;
    let mut rest = r;
    for t in [&mut orig, &mut rest] {
        t.feed(b"\x1b[24;79Habcd");
    }
    assert_ne!(
        orig.grid(),
        rest.grid(),
        "DECAWM state is outside the snapshot"
    );
}

fn check_delta(base: &Session, next: &Session) {
    let (bg, ng) = (base.screen(), next.screen());
    let d = Delta::between(&bg, &ng);
    let mut copy: Grid = bg.clone();
    d.apply(&mut copy);
    assert_eq!(copy, ng, "apply");
    // The VT encoding, fed to a peer restored from the baseline, reproduces the screen.
    let mut peer = restore(&base.snapshot(), bg.cols, bg.rows, 0);
    peer.feed(&d.to_vt());
    let pg = peer.screen();
    for (y, (a, b)) in pg.screen.iter().zip(&ng.screen).enumerate() {
        assert_eq!(a.cells, b.cells, "to_vt row {y}");
    }
    assert_eq!(pg.cursor, ng.cursor, "to_vt cursor");
}

#[test]
fn delta_reproduces_the_screen_over_random_edits() {
    let mut s = fed(200, 50, 0, &text_corpus(60));
    for seed in 1..=20u64 {
        let base = restore(&s.snapshot(), 200, 50, 0);
        s.feed(&edit_corpus(seed.wrapping_mul(0x2545_f491_4f6c_dd1d), 4096));
        check_delta(&base, &s);
        s = restore(&s.snapshot(), 200, 50, 0);
    }
}

#[test]
fn delta_carries_resize_and_is_empty_when_unchanged() {
    let s = fed(80, 24, 0, b"hello\r\nworld");
    let g = s.screen();
    assert!(Delta::between(&g, &g).is_empty(&g));
    let mut t = fed(80, 24, 0, b"hello\r\nworld");
    t.resize(100, 30);
    let d = Delta::between(&g, &t.screen());
    assert!(d.resized);
    let mut copy = g.clone();
    d.apply(&mut copy);
    assert_eq!(copy, t.screen());
}

#[test]
fn reflow_200x50_to_80x24_and_back() {
    let mut d = String::new();
    for i in 0..30 {
        let line: String = (0..150)
            .map(|k| (b'a' + ((i + k) % 26) as u8) as char)
            .collect();
        d.push_str(&line);
        d.push_str("\r\n");
    }
    let mut s = fed(200, 50, 10_000, d.as_bytes());
    let all = |s: &Session| -> Vec<String> {
        s.grid()
            .history
            .iter()
            .chain(s.grid().screen.iter())
            .map(|r| r.text())
            .collect()
    };
    let logical = |t: &[String]| {
        t.iter()
            .filter(|l| !l.is_empty())
            .cloned()
            .collect::<String>()
    };
    let before = all(&s);
    s.resize(80, 24);
    assert_eq!(s.size(), (80, 24));
    let mid = all(&s);
    assert_eq!(
        mid.iter().filter(|l| !l.is_empty()).count(),
        60,
        "each 150-char line rewraps to 2 rows"
    );
    assert_eq!(
        logical(&mid),
        logical(&before),
        "no characters lost at 80 columns"
    );
    s.resize(200, 50);
    let after = all(&s);
    assert_eq!(
        after.iter().filter(|l| l.len() == 150).count(),
        30,
        "lines rejoined"
    );
    assert_eq!(logical(&after), logical(&before));
}

//! The spike's conformance probes that rio-vt passed (each on a fresh 80x24 session), the ED 1
//! regression, events, replies and damage.
mod common;

use common::{fed, line};
use crucible_term_core::{Color, Damage, Event, PromptMark, Session, Underline, Width};
use std::path::PathBuf;

fn probe(input: &str) -> Session {
    fed(80, 24, 1000, input.as_bytes())
}

fn col_of(s: &Session, y: usize, text: &str) -> Option<usize> {
    s.screen_row(y).cells.iter().position(|c| c.text == text)
}

#[test]
fn ed1_from_row_2_clears_row_1() {
    let s = probe("AAAA\r\nBBBB\x1b[2;2H\x1b[1J");
    assert_eq!(
        line(&s, 0),
        "",
        "row 0 cleared (rio-vt's inherited bug left AAAA)"
    );
    assert_eq!(line(&s, 1), "  BB");
}

#[test]
fn ech_el_ed1_probe() {
    let s = probe("0123456789\x1b[1;3H\x1b[2X\x1b[1;8H\x1b[K\r\nline2\x1b[1J");
    assert_eq!((line(&s, 0), line(&s, 1)), (String::new(), String::new()));
}

#[test]
fn ed1_from_deeper_rows_and_row_0_unchanged_behaviour() {
    let s = probe("AAAA\r\nBBBB\r\nCCCC\x1b[3;2H\x1b[1J");
    assert_eq!(
        (line(&s, 0), line(&s, 1), line(&s, 2)),
        (String::new(), String::new(), "  CC".into())
    );
    let s = probe("AAAA\r\nBBBB\x1b[1;2H\x1b[1J");
    assert_eq!((line(&s, 0), line(&s, 1)), ("  AA".into(), "BBBB".into()));
}

#[test]
fn scroll_region_origin_insert_and_cursor_save_probes() {
    let s = probe("1\r\n2\r\n3\r\n4\r\n5\x1b[2;4r\x1b[4;1H\n");
    let t: Vec<String> = (0..5).map(|y| line(&s, y)).collect();
    assert_eq!(t, ["1", "3", "4", "", "5"], "DECSTBM");
    let s = probe("\x1b[5;10r\x1b[?6h\x1b[1;1HX\x1b[?6l\x1b[r");
    assert_eq!(line(&s, 4), "X", "DECOM");
    let s = probe("abcdef\r\x1b[4hXY\x1b[4l");
    assert_eq!(line(&s, 0), "XYabcdef", "IRM");
    let s = probe("\x1b[3;7H\x1b7\x1b[10;10H\x1b8Z");
    assert_eq!(s.screen_row(2).cells[6].text, "Z", "DECSC/DECRC");
    let s = probe("primary\x1b[?1049h\x1b[2Jalt\x1b[?1049l");
    assert_eq!(
        (line(&s, 0), s.cursor()),
        ("primary".into(), (7, 0)),
        "1049"
    );
    let s = probe("\x1b[?7l\x1b[1;79Habcd");
    assert!(
        line(&s, 0).ends_with("ad") && line(&s, 1).is_empty(),
        "DECAWM off"
    );
}

#[test]
fn edit_and_tab_probes() {
    assert_eq!(
        line(&probe("abcdef\x1b[1;2H\x1b[2@\x1b[1;5H\x1b[1P"), 0),
        "a  bdef",
        "ICH/DCH"
    );
    assert_eq!(line(&probe("x\x1b[4b"), 0), "xxxxx", "REP");
    assert_eq!(
        col_of(
            &probe("\x1b[3g\x1b[1;5H\x1bH\x1b[1;1H\tA\x1b[2;1H\x1b[0g"),
            0,
            "A"
        ),
        Some(4),
        "HTS/TBC"
    );
    assert_eq!(col_of(&probe("\tA"), 0, "A"), Some(8), "default tabs");
}

#[test]
fn width_probes() {
    let s = probe("\x1b[1;80H漢");
    let c = &s.screen_row(1).cells[0];
    assert_eq!(
        (c.text.as_str(), c.width),
        ("漢", Width::Wide),
        "wide at margin wraps"
    );
    let s = probe("漢\u{301}x");
    assert_eq!(
        (
            s.screen_row(0).cells[0].text.as_str(),
            s.screen_row(0).cells[2].text.as_str()
        ),
        ("漢\u{301}", "x")
    );
    let s = probe("e\u{301}x");
    assert_eq!(
        (
            s.screen_row(0).cells[0].text.as_str(),
            s.screen_row(0).cells[1].text.as_str()
        ),
        ("e\u{301}", "x")
    );
    for (input, what) in [
        ("👨\u{200d}👩\u{200d}👧|", "ZWJ"),
        ("\x1b[?2027h👨\u{200d}👩\u{200d}👧|", "ZWJ 2027"),
        ("❤\u{fe0f}|", "VS16"),
        ("🇩🇪|", "flag"),
    ] {
        assert_eq!(probe(input).cursor(), (3, 0), "{what}");
    }
}

#[test]
fn sgr_probes() {
    let s = probe("\x1b[38;2;1;2;3mT");
    assert_eq!(s.screen_row(0).cells[0].attrs.fg, Color::Rgb(1, 2, 3));
    let s = probe("\x1b[4:3mU");
    assert_eq!(s.screen_row(0).cells[0].attrs.underline, Underline::Curly);
}

#[test]
fn osc_events() {
    let mut s = probe(
        "\x1b]2;hello title\x07\x1b]0;other\x07\x1b]7;file:///tmp/x\x07\x1b]52;c;aGVsbG8=\x07\x1b]52;p;?\x07\x07",
    );
    assert_eq!(
        s.take_events(),
        [
            Event::Title("hello title".into()),
            Event::Title("other".into()),
            Event::Cwd(PathBuf::from("/tmp/x")),
            Event::ClipboardStore {
                primary: false,
                text: "hello".into()
            },
            Event::ClipboardLoad { primary: true },
            Event::Bell,
        ]
    );
    assert_eq!(s.cwd(), Some(std::path::Path::new("/tmp/x")));
    assert!(s.take_events().is_empty(), "drained");
}

#[test]
fn osc133_prompt_marks_ride_with_rows_into_scrollback() {
    let mut s = Session::new(80, 3, 100);
    s.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    assert_eq!(s.screen_row(0).prompt, PromptMark::Prompt);
    assert_eq!(s.screen_row(1).prompt, PromptMark::None);
    assert_eq!(s.screen_row(2).prompt, PromptMark::Prompt);
    s.feed(b"\r\n\r\n");
    assert_eq!(s.history_row(0).prompt, PromptMark::Prompt);
    s.feed(b"\x1b]133;P;k=c\x07> ");
    assert_eq!(s.screen_row(2).prompt, PromptMark::Continuation);
}

#[test]
fn replies_for_the_child() {
    let reply = |input: &str| {
        let mut s = probe(input);
        String::from_utf8(s.take_replies()).unwrap()
    };
    assert!(reply("\x1b[c").starts_with("\x1b[?"), "DA1");
    assert_eq!(reply("\x1b[3;5H\x1b[6n"), "\x1b[3;5R", "DSR CPR");
    assert!(
        reply("\x1b[?2004h\x1b[?2004$p").contains("2004;1$y"),
        "DECRQM 2004"
    );
    assert!(
        reply("\x1b[?1004h\x1b[?1004$p").contains("1004;1$y"),
        "DECRQM 1004"
    );
    assert!(
        reply("\x1b[?2026h\x1b[?2026$p").contains("2026;1$y"),
        "DECRQM 2026"
    );
    assert!(
        reply("\x1b[>5u\x1b[?u").contains("\x1b[?5u"),
        "kitty keyboard"
    );
    assert!(reply("\x1b[>q").contains("\x1bP>|"), "XTVERSION");
    assert!(
        reply("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\").contains("OK"),
        "kitty graphics query"
    );
}

#[test]
fn damage_is_line_level_and_resets() {
    let mut s = probe("");
    let _ = s.take_damage();
    s.feed(b"\x1b[5;1Hx");
    match s.take_damage() {
        Damage::Rows(r) => assert!(r.contains(&4) && r.len() <= 2, "{r:?}"),
        Damage::Full => panic!("expected partial damage"),
    }
    assert_eq!(s.take_damage(), Damage::Rows(vec![]));
    s.resize(100, 30);
    assert_eq!(s.take_damage(), Damage::Full);
}

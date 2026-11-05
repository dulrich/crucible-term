//! Golden tests of the paint function, plus the `#[ignore]` step-0 swash benchmark.
//!
//! Text goldens use DejaVu Sans Mono located once with `fc-match` (the test's only font-config
//! dependency); pixel checksums live in `tests/golden/*.fnv`. Re-bless with
//! `CRUCIBLE_TERM_BLESS=1 cargo test` (also writes `target/golden-*.ppm` for inspection).

use super::*;
use crate::font::fc::Located;
use crate::font::{Face, Fallback, Fonts};
use crucible_term_core::Session;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fc_match(pattern: &str) -> Located {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}\n%{index}", pattern])
        .output()
        .expect("fc-match");
    let s = String::from_utf8(out.stdout).expect("utf8");
    let mut it = s.lines();
    Located {
        path: PathBuf::from(it.next().expect("file")),
        index: it.next().and_then(|i| i.parse().ok()).unwrap_or(0),
    }
}

fn face(loc: &Located) -> Face {
    Face::from_bytes(std::fs::read(&loc.path).expect("read font"), loc.index).expect("parse")
}

/// Fallback for tests: CJK and emoji via `fc-match`, nothing from the user's config otherwise.
struct TestFallback;

impl Fallback for TestFallback {
    fn locate(&mut self, ch: char, color: bool) -> Option<Located> {
        if color {
            return Some(fc_match("Noto Color Emoji"));
        }
        Some(fc_match(&format!(":charset={:x}", ch as u32)))
    }
}

fn fonts(fallback: bool) -> Fonts {
    let faces = vec![
        face(&fc_match("DejaVu Sans Mono")),
        face(&fc_match("DejaVu Sans Mono:bold")),
        face(&fc_match("DejaVu Sans Mono:oblique")),
        face(&fc_match("DejaVu Sans Mono:bold:oblique")),
    ];
    let fb: Option<Box<dyn Fallback>> = fallback.then(|| Box::new(TestFallback) as _);
    Fonts::from_faces(faces, [0, 1, 2, 3], fb, 11.0, 1.0)
}

const FOCUSED: CursorDraw = CursorDraw {
    style: CursorStyle {
        shape: CursorKind::Block,
        blinking: false,
    },
    focused: true,
    blink_on: true,
};

const NO_CURSOR: CursorDraw = CursorDraw {
    blink_on: false,
    ..FOCUSED
};

struct Out {
    px: Vec<u32>,
    w: usize,
    h: usize,
}

fn render(cols: usize, rows: usize, input: &[u8], f: &mut Fonts, cur: CursorDraw, pad: u32) -> Out {
    let mut s = Session::new(cols, rows, 0);
    s.feed(input);
    let grid = s.screen();
    for row in &grid.screen {
        for c in &row.cells {
            f.prepare(&c.text);
        }
    }
    let m = f.metrics();
    let (w, h) = (
        cols * m.cell_w as usize + 2 * pad as usize,
        rows * m.cell_h as usize + 2 * pad as usize,
    );
    let mut px = vec![0u32; w * h];
    let mut c = Canvas {
        px: &mut px,
        width: w,
        height: h,
    };
    paint(&mut c, &grid, &Theme::default(), f, pad, cur, None, None);
    Out { px, w, h }
}

fn fnv(px: &[u32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for p in px {
        for b in p.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn golden(name: &str, o: &Out) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let file = dir.join(format!("{name}.fnv"));
    let got = format!("{:016x}\n", fnv(&o.px));
    if std::env::var_os("CRUCIBLE_TERM_BLESS").is_some() {
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(&file, &got).expect("write golden");
        let mut ppm = format!("P6\n{} {}\n255\n", o.w, o.h).into_bytes();
        for p in &o.px {
            ppm.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
        }
        let t = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::write(t.join(format!("golden-{name}.ppm")), ppm).expect("write ppm");
        return;
    }
    let want = std::fs::read_to_string(&file).unwrap_or_default();
    assert_eq!(got, want, "golden {name} differs (re-bless if intended)");
}

fn at(o: &Out, x: usize, y: usize) -> u32 {
    o.px[y * o.w + x]
}

#[test]
fn padding_and_background_alpha_are_premultiplied() {
    let mut f = fonts(false);
    let mut s = Session::new(4, 2, 0);
    s.feed(b"\x1b[41m \x1b[0m");
    let grid = s.screen();
    let m = f.metrics();
    let pad = 3;
    let (w, h) = (4 * m.cell_w as usize + 6, 2 * m.cell_h as usize + 6);
    let mut px = vec![0u32; w * h];
    let theme = Theme {
        alpha: 0x80,
        ..Theme::default()
    };
    let mut c = Canvas {
        px: &mut px,
        width: w,
        height: h,
    };
    paint(&mut c, &grid, &theme, &mut f, pad, NO_CURSOR, None, None);
    // Default bg #111111 at alpha 0x80, premultiplied: 0x11 * 0x80 / 255 = 0x09.
    assert_eq!(px[0], 0x8009_0909, "padding corner");
    assert_eq!(px[(h - 1) * w + w - 1], 0x8009_0909, "padding far corner");
    // An explicit background (red, index 1) is opaque.
    assert_eq!(px[(pad as usize + 1) * w + pad as usize + 1], 0xfff6_2b5a);
}

#[test]
fn colours_indexed_rgb_inverse() {
    let mut f = fonts(false);
    let o = render(
        4,
        1,
        b"\x1b[48;5;196m \x1b[48;2;1;2;3m \x1b[7m \x1b[0m",
        &mut f,
        NO_CURSOR,
        0,
    );
    let m = f.metrics();
    let mid = |c: usize| at(&o, c * m.cell_w as usize + 1, m.cell_h as usize / 2);
    assert_eq!(mid(0), 0xffff_0000, "palette 196");
    assert_eq!(mid(1), 0xff01_0203, "truecolor");
    assert_eq!(mid(2), 0xffdc_dccc, "inverse: default fg as bg");
    assert_eq!(mid(3), 0xff11_1111, "default bg");
}

#[test]
fn golden_text_styles() {
    let mut f = fonts(false);
    let o = render(
        10,
        3,
        b"Hello \x1b[1mBold\r\n\x1b[0;3mItal \x1b[2mfaint\r\n\x1b[0;9mstrike\x1b[0;31;1m!",
        &mut f,
        NO_CURSOR,
        2,
    );
    golden("text_styles", &o);
}

#[test]
fn golden_underline_styles() {
    let mut f = fonts(false);
    let o = render(
        10,
        5,
        b"\x1b[4:1msingle\r\n\x1b[4:2mdouble\r\n\x1b[4:3mcurly\r\n\x1b[4:4mdotted\r\n\x1b[4:5mdashed",
        &mut f,
        NO_CURSOR,
        0,
    );
    golden("underline_styles", &o);
}

#[test]
fn cursor_shapes_and_blink() {
    let mut f = fonts(false);
    let m = f.metrics();
    let (cw, ch) = (m.cell_w as usize, m.cell_h as usize);
    let cursor = 0xffdc_dccc;
    let shape = |k| CursorDraw {
        style: CursorStyle {
            shape: k,
            blinking: true,
        },
        ..FOCUSED
    };
    // Block: the whole cell (blank) is cursor coloured.
    let o = render(3, 1, b"", &mut f, shape(CursorKind::Block), 0);
    assert_eq!(at(&o, cw / 2, ch / 2), cursor);
    // Beam: left column only.
    let o = render(3, 1, b"", &mut f, shape(CursorKind::Beam), 0);
    assert_eq!(at(&o, 0, ch / 2), cursor);
    assert_eq!(at(&o, cw / 2, ch / 2), 0xff11_1111);
    // Underline: at the underline row, not the middle.
    let o = render(3, 1, b"", &mut f, shape(CursorKind::Underline), 0);
    assert_eq!(at(&o, cw / 2, m.underline_y as usize), cursor);
    assert_eq!(at(&o, cw / 2, ch / 2), 0xff11_1111);
    // Unfocused: hollow block — edges coloured, centre not.
    let o = render(
        3,
        1,
        b"",
        &mut f,
        CursorDraw {
            focused: false,
            ..FOCUSED
        },
        0,
    );
    assert_eq!(at(&o, 0, 0), cursor);
    assert_eq!(at(&o, cw - 1, ch - 1), cursor);
    assert_eq!(at(&o, cw / 2, ch / 2), 0xff11_1111);
    // Blink off and hidden: nothing.
    let o = render(3, 1, b"", &mut f, NO_CURSOR, 0);
    assert!(o.px.iter().all(|&p| p == 0xff11_1111));
    let o = render(3, 1, b"", &mut f, shape(CursorKind::Hidden), 0);
    assert!(o.px.iter().all(|&p| p == 0xff11_1111));
    // Block over a glyph inverts it: some pixels in the cell are the background colour.
    let o = render(3, 1, b"M\x1b[D", &mut f, FOCUSED, 0);
    assert!((0..cw).any(|x| at(&o, x, ch / 2) == 0xff11_1111));
    assert!((0..cw).any(|x| at(&o, x, ch / 2) == cursor));
}

#[test]
fn wide_cjk_combining_and_colour_emoji() {
    let mut f = fonts(true);
    let m = f.metrics();
    let (cw, ch) = (m.cell_w as usize, m.cell_h as usize);
    // "中" spans columns 0-1; its ink reaches into the second cell.
    let o = render(6, 1, "中".as_bytes(), &mut f, NO_CURSOR, 0);
    assert!((cw + 1..2 * cw).any(|x| (0..ch).any(|y| at(&o, x, y) != 0xff11_1111)));
    // "e" + U+0301: the combining acute adds ink above the plain "e".
    let plain = render(2, 1, b"e", &mut f, NO_CURSOR, 0);
    let acute = render(2, 1, "e\u{301}".as_bytes(), &mut f, NO_CURSOR, 0);
    let ink = |o: &Out| o.px.iter().filter(|&&p| p != 0xff11_1111).count();
    assert!(ink(&acute) > ink(&plain), "combining mark drawn");
    // A ZWJ sequence (rainbow flag) is one colour glyph: some ink is neither grey nor background.
    let o = render(4, 1, "🏳\u{fe0f}\u{200d}🌈".as_bytes(), &mut f, NO_CURSOR, 0);
    let coloured = o.px.iter().any(|&p| {
        let (r, g, b) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
        r.abs_diff(g) > 40 || g.abs_diff(b) > 40
    });
    assert!(coloured, "colour emoji drawn");
}

#[test]
fn damage_limited_repaint_matches_full_and_touches_only_its_rows() {
    let mut f = fonts(false);
    let mut s = Session::new(10, 3, 0);
    s.feed(b"row zero\r\nrow one\r\nrow two");
    let _ = s.take_damage();
    let m = f.metrics();
    let (w, h) = (10 * m.cell_w as usize, 3 * m.cell_h as usize);
    let theme = Theme::default();
    let mut inc = vec![0u32; w * h];
    let first = s.screen();
    paint(
        &mut Canvas {
            px: &mut inc,
            width: w,
            height: h,
        },
        &first,
        &theme,
        &mut f,
        0,
        FOCUSED,
        None,
        None,
    );
    let before = inc.clone();
    s.feed(b"\x1b[2;1HROW");
    let rows = rows_to_paint(&s.take_damage(), Some(first.cursor.1), s.cursor().1).expect("rows");
    assert_eq!(rows, vec![1, 2]);
    let next = s.screen();
    let bands = paint(
        &mut Canvas {
            px: &mut inc,
            width: w,
            height: h,
        },
        &next,
        &theme,
        &mut f,
        0,
        FOCUSED,
        Some(&rows),
        None,
    );
    assert_eq!(
        bands,
        vec![
            (m.cell_h as usize, m.cell_h as usize),
            (2 * m.cell_h as usize, m.cell_h as usize)
        ]
    );
    assert_eq!(
        &inc[..w * m.cell_h as usize],
        &before[..w * m.cell_h as usize],
        "row 0 untouched"
    );
    let mut full = vec![0u32; w * h];
    paint(
        &mut Canvas {
            px: &mut full,
            width: w,
            height: h,
        },
        &next,
        &theme,
        &mut f,
        0,
        FOCUSED,
        None,
        None,
    );
    assert_eq!(inc, full, "incremental == full repaint");
}

#[test]
fn grid_size_rounds_down_to_whole_cells() {
    let f = fonts(false);
    let m = f.metrics();
    let (w, h) = (m.cell_w * 80 + 2 * 5 + m.cell_w - 1, m.cell_h * 24 + 2 * 5);
    assert_eq!(grid_size(w, h, &m, 5), (80, 24));
    assert_eq!(grid_size(1, 1, &m, 5), (1, 1));
}

#[test]
fn selected_cells_swap_colours_and_pointer_maps_to_cells() {
    let mut f = fonts(false);
    let mut sess = Session::new(3, 1, 0);
    sess.feed(b"   ");
    let grid = sess.screen();
    let m = f.metrics();
    let (w, h) = (3 * m.cell_w as usize, m.cell_h as usize);
    let mut px = vec![0u32; w * h];
    let sel = |_r: usize, c: usize| c == 1;
    paint(
        &mut Canvas {
            px: &mut px,
            width: w,
            height: h,
        },
        &grid,
        &Theme::default(),
        &mut f,
        0,
        NO_CURSOR,
        None,
        Some(&sel),
    );
    let o = Out { px, w, h };
    let mid = |c: usize| at(&o, c * m.cell_w as usize + 1, m.cell_h as usize / 2);
    assert_eq!(mid(0), 0xff11_1111, "unselected default bg");
    assert_eq!(mid(1), 0xffdc_dccc, "selected: fg shows as bg");
    // Pointer mapping: padding aware, clamped to the grid.
    let (cw, ch) = (f64::from(m.cell_w), f64::from(m.cell_h));
    assert_eq!(
        cell_at(4.0 + cw * 2.5, 4.0 + ch * 1.5, &m, 4, 80, 24),
        (2, 1)
    );
    assert_eq!(cell_at(-9.0, 1.0, &m, 4, 80, 24), (0, 0));
    assert_eq!(cell_at(1e6, 1e6, &m, 4, 80, 24), (79, 23));
}

/// Step 0 (plan 01 WP-2): swash shaped + rasterized 200x50 mixed-content frame, cold and warm.
/// `cargo test --release -p crucible-term -- --ignored bench_swash --nocapture`
#[test]
#[ignore = "benchmark, not a gate"]
fn bench_swash_frame() {
    use std::time::Instant;
    let (cols, rows) = (200usize, 50usize);
    let mut line = String::new();
    let parts = [
        "hello, world ",
        "中文字符 ",
        "🏳\u{fe0f}\u{200d}🌈 👩\u{200d}👩\u{200d}👧 ",
        "e\u{301}a\u{308} ",
        "fn main() {} ",
    ];
    while line.chars().count() < cols * 3 {
        for p in parts {
            line.push_str(p);
        }
    }
    let mut input = Vec::new();
    for r in 0..rows {
        input.extend_from_slice(format!("\x1b[{};1H\x1b[{}m", r + 1, 31 + r % 7).as_bytes());
        let skip = r * 3 % 20;
        input.extend(
            line.chars()
                .skip(skip)
                .take(cols - 2)
                .collect::<String>()
                .bytes(),
        );
    }
    let mut s = Session::new(cols, rows, 0);
    s.feed(&input);
    let grid = s.screen();
    let mut f = fonts(true);
    let t0 = Instant::now();
    for row in &grid.screen {
        for c in &row.cells {
            f.prepare(&c.text);
        }
    }
    let prep = t0.elapsed();
    let m = f.metrics();
    let (w, h) = (cols * m.cell_w as usize, rows * m.cell_h as usize);
    let mut px = vec![0u32; w * h];
    let theme = Theme::default();
    let mut frame = |f: &mut Fonts| {
        let t = Instant::now();
        paint(
            &mut Canvas {
                px: &mut px,
                width: w,
                height: h,
            },
            &grid,
            &theme,
            f,
            0,
            FOCUSED,
            None,
            None,
        );
        t.elapsed().as_secs_f64() * 1e3
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let runs = 9;
    let cold = median(
        (0..runs)
            .map(|_| {
                f.clear_caches();
                frame(&mut f)
            })
            .collect(),
    );
    let warm_reshape = median(
        (0..runs)
            .map(|_| {
                f.clear_shape_cache();
                frame(&mut f)
            })
            .collect(),
    );
    let warm = median((0..runs).map(|_| frame(&mut f)).collect());
    eprintln!(
        "swash 200x50 {w}x{h}px cell {}x{} px {:.2}: fallback prepare {:.1} ms (once); cold {cold:.2} ms; warm (reshape every cell, raster cached) {warm_reshape:.2} ms; warm (all cached) {warm:.2} ms; median of {runs}",
        m.cell_w,
        m.cell_h,
        m.px,
        prep.as_secs_f64() * 1e3
    );
}

#[test]
fn centered_splits_the_spare_pixels() {
    let f = fonts(false);
    let m = f.metrics();
    let (cw, ch) = (m.cell_w, m.cell_h);
    let p = centered(cw * 10 + 12, ch * 4 + 7, &m, (10, 4), Pad { x: 2, y: 1 });
    // 12 spare px minus 2*2 padding = 8, so 4 more each side; 7 - 2 = 5, so 2 more (floor).
    assert_eq!(p, Pad { x: 6, y: 3 });
    assert_eq!(
        centered(1, 1, &m, (10, 4), Pad { x: 2, y: 1 }),
        Pad { x: 2, y: 1 }
    );
}

/// The `CTERM_DEBUG_FRAME_SUM` checksum of an incrementally painted buffer equals the
/// forced-full-repaint checksum of the same grid (what WP-8 compares after un-hiding).
#[test]
fn frame_sum_of_incremental_paint_equals_full_repaint() {
    use crate::window::lifecycle::fnv1a64_px;
    let mut f = fonts(false);
    let mut s = Session::new(8, 2, 0);
    s.feed(b"abc\r\ndef");
    let m = f.metrics();
    let (w, h) = (8 * m.cell_w as usize, 2 * m.cell_h as usize);
    let theme = Theme::default();
    let mut paint_into = |px: &mut [u32], g: &crucible_term_core::Grid, rows: Option<&[usize]>| {
        paint(
            &mut Canvas {
                px,
                width: w,
                height: h,
            },
            g,
            &theme,
            &mut f,
            0,
            FOCUSED,
            rows,
            None,
        );
    };
    let mut inc = vec![0u32; w * h];
    let first = s.screen();
    paint_into(&mut inc, &first, None);
    let _ = s.take_damage();
    s.feed(b"\x1b[1;1HXY");
    let rows = rows_to_paint(&s.take_damage(), Some(first.cursor.1), s.cursor().1).expect("rows");
    let next = s.screen();
    paint_into(&mut inc, &next, Some(&rows));
    let mut full = vec![0u32; w * h];
    paint_into(&mut full, &next, None);
    assert_eq!(fnv1a64_px(&inc), fnv1a64_px(&full));
    assert_ne!(fnv1a64_px(&inc), fnv1a64_px(&vec![0u32; w * h]));
}

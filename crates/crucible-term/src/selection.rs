//! The selection model: a pure state machine over absolute grid coordinates (row 0 = the oldest
//! history row, then the screen), so a selection survives scrolling. Text extraction and
//! hit-testing take rows through a lookup closure; nothing here touches Wayland or the session.
//!
//! Kinds: character (click-drag), word (double-click), line (triple-click; follows soft-wrapped
//! rows) and block (Ctrl+drag; Alt is left to the child's Alt-click convention).

use crucible_term_core::{Damage, Row, Width};

/// Characters that end a word, besides whitespace and blank cells: `,│`|:"'()[]{}<>` (foot's
/// default `word-delimiters`).
pub const WORD_DELIMITERS: &str = ",│`|:\"'()[]{}<>";

/// A cell in absolute grid coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    /// Absolute row (0 = oldest history row).
    pub row: usize,
    /// Column.
    pub col: usize,
}

/// What a drag selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Cell to cell, in reading order.
    Char,
    /// Whole words.
    Word,
    /// Whole (soft-wrapped) lines.
    Line,
    /// A rectangle.
    Block,
}

/// Row lookup by absolute index.
pub type Rows<'a> = &'a dyn Fn(usize) -> Option<Row>;

/// A selection in progress or finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    kind: Kind,
    /// The span the gesture started on (a single cell for char/block, the word or line otherwise).
    anchor: (Point, Point),
    /// The span under the pointer now.
    head: (Point, Point),
}

fn is_blank(row: &Row, col: usize) -> bool {
    row.cells.get(col).is_none_or(|c| c.text.is_empty())
}

fn is_delim(row: &Row, col: usize) -> bool {
    row.cells.get(col).is_none_or(|c| {
        c.text.is_empty()
            || c.text
                .chars()
                .all(|ch| ch.is_whitespace() || WORD_DELIMITERS.contains(ch))
    })
}

/// The leading column of the wide glyph covering `col`.
fn lead_col(row: &Row, col: usize) -> usize {
    if col > 0 && row.cells.get(col).is_some_and(|c| c.width == Width::Spacer) {
        col - 1
    } else {
        col
    }
}

/// The span `kind` selects around `p`.
fn span(kind: Kind, p: Point, rows: Rows<'_>) -> (Point, Point) {
    let Some(row) = rows(p.row) else {
        return (p, p);
    };
    let col = lead_col(&row, p.col);
    match kind {
        Kind::Char | Kind::Block => {
            let end = if row.cells.get(col).is_some_and(|c| c.width == Width::Wide) {
                col + 1
            } else {
                col
            };
            (
                Point { row: p.row, col },
                Point {
                    row: p.row,
                    col: end,
                },
            )
        }
        Kind::Word => {
            let class = is_delim(&row, col);
            // Blank runs and delimiter runs select as a unit, like words.
            let same = |c: usize| {
                if class {
                    is_blank(&row, c) == is_blank(&row, col) && is_delim(&row, c)
                } else {
                    !is_delim(&row, c)
                }
            };
            let mut a = col;
            while a > 0 && same(a - 1) {
                a -= 1;
            }
            let mut b = col;
            while b + 1 < row.cells.len() && same(b + 1) {
                b += 1;
            }
            (Point { row: p.row, col: a }, Point { row: p.row, col: b })
        }
        Kind::Line => {
            let mut top = p.row;
            while top > 0 && rows(top - 1).is_some_and(|r| r.wrapped) {
                top -= 1;
            }
            let mut bot = p.row;
            while rows(bot).is_some_and(|r| r.wrapped) && rows(bot + 1).is_some() {
                bot += 1;
            }
            let last = row.cells.len().saturating_sub(1);
            (
                Point { row: top, col: 0 },
                Point {
                    row: bot,
                    col: last,
                },
            )
        }
    }
}

impl Selection {
    /// Begin a selection of `kind` at `p`.
    pub fn start(kind: Kind, p: Point, rows: Rows<'_>) -> Selection {
        let s = span(kind, p, rows);
        Selection {
            kind,
            anchor: s,
            head: s,
        }
    }

    /// Extend the selection to the pointer at `p`.
    pub fn update(&mut self, p: Point, rows: Rows<'_>) {
        self.head = span(self.kind, p, rows);
    }

    /// Normalized `(start, end)` in reading order, end inclusive.
    pub fn range(&self) -> (Point, Point) {
        let lo = self.anchor.0.min(self.head.0);
        let hi = self.anchor.1.max(self.head.1);
        (lo, hi)
    }

    /// For a block selection the column bounds, else the rows' own.
    fn block_cols(&self) -> (usize, usize) {
        let cols = [
            self.anchor.0.col,
            self.anchor.1.col,
            self.head.0.col,
            self.head.1.col,
        ];
        (
            cols.iter().copied().min().unwrap_or(0),
            cols.iter().copied().max().unwrap_or(0),
        )
    }

    /// True if the cell at absolute (`row`, `col`) is selected.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let (lo, hi) = self.range();
        if row < lo.row || row > hi.row {
            return false;
        }
        if self.kind == Kind::Block {
            let (a, b) = self.block_cols();
            return (a..=b).contains(&col);
        }
        let p = Point { row, col };
        lo <= p && p <= hi
    }

    /// True if any selected row is among `rows` (absolute indices).
    pub fn touches(&self, rows: std::ops::Range<usize>) -> bool {
        let (lo, hi) = self.range();
        lo.row < rows.end && hi.row >= rows.start
    }

    /// The selected text: wide glyphs once, spacers skipped, trailing blanks trimmed per line,
    /// a newline between rows except after a soft-wrapped row (never for block).
    pub fn text(&self, rows: Rows<'_>) -> String {
        let (lo, hi) = self.range();
        let (bc0, bc1) = self.block_cols();
        let mut out = String::new();
        for r in lo.row..=hi.row {
            let Some(row) = rows(r) else { break };
            let last_col = row.cells.len().saturating_sub(1);
            let (c0, c1) = match self.kind {
                Kind::Block => (bc0, bc1.min(last_col)),
                _ => (
                    if r == lo.row { lo.col } else { 0 },
                    if r == hi.row {
                        hi.col.min(last_col)
                    } else {
                        last_col
                    },
                ),
            };
            let mut line = String::new();
            for c in c0..=c1 {
                let Some(cell) = row.cells.get(c) else { break };
                match (cell.width, cell.text.is_empty()) {
                    (Width::Spacer, _) => {}
                    (_, true) => line.push(' '),
                    (_, false) => line.push_str(&cell.text),
                }
            }
            let joined = self.kind != Kind::Block && row.wrapped && r != hi.row;
            if !joined {
                line.truncate(line.trim_end_matches(' ').len());
            }
            out.push_str(&line);
            if r != hi.row && !joined {
                out.push('\n');
            }
        }
        out
    }
}

/// Merge two damages (a `Full` absorbs everything).
pub fn merge_damage(a: Damage, b: Damage) -> Damage {
    match (a, b) {
        (Damage::Rows(mut x), Damage::Rows(y)) => {
            x.extend(y);
            x.sort_unstable();
            x.dedup();
            Damage::Rows(x)
        }
        _ => Damage::Full,
    }
}

/// True if output with `damage` (visible-row indices) changed rows the selection covers, or
/// scrolled the history (`history_changed`): such a selection is stale and must be cleared.
pub fn invalidated(
    sel: &Selection,
    damage: &Damage,
    history_len: usize,
    history_changed: bool,
) -> bool {
    if history_changed {
        return true;
    }
    match damage {
        Damage::Full => true,
        Damage::Rows(rows) => rows
            .iter()
            .any(|&y| sel.touches(history_len + y..history_len + y + 1)),
    }
}

/// Multi-click tracking: the number of consecutive clicks (1-3) at one cell within `window`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clicks {
    last: Option<(std::time::Instant, Point, u8)>,
}

impl Clicks {
    /// Register a press at `p` at `now`; returns 1 (single), 2 (double) or 3 (triple, then wraps).
    pub fn press(&mut self, now: std::time::Instant, p: Point, window: std::time::Duration) -> u8 {
        let n = match self.last {
            Some((t, q, n)) if q == p && now.duration_since(t) <= window => n % 3 + 1,
            _ => 1,
        };
        self.last = Some((now, p, n));
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_term_core::Session;
    use std::time::{Duration, Instant};

    fn session(input: &str, cols: usize) -> Session {
        let mut s = Session::new(cols, 4, 50);
        s.feed(input.as_bytes());
        s
    }

    fn text_of(s: &Session, sel: &Selection) -> String {
        sel.text(&|r| s.abs_row(r))
    }

    fn pt(row: usize, col: usize) -> Point {
        Point { row, col }
    }

    #[test]
    fn char_drag_trims_trailing_blanks_and_orders_rows() {
        let s = session("hello world\r\nsecond   \r\nthird", 20);
        let rows = |r| s.abs_row(r);
        let mut sel = Selection::start(Kind::Char, pt(1, 3), &rows);
        sel.update(pt(0, 6), &rows); // dragged backwards across rows
        assert_eq!(text_of(&s, &sel), "world\nseco");
        assert!(sel.contains(0, 6) && sel.contains(1, 3) && !sel.contains(1, 4));
        assert!(!sel.contains(0, 5));
        // Trailing blanks of a middle line are dropped.
        sel.update(pt(2, 2), &rows);
        assert_eq!(text_of(&s, &sel), "ond\nthi");
    }

    #[test]
    fn word_and_line_expand_and_follow_wraps() {
        let s = session("foo(bar_baz) qux", 8); // wraps at 8 columns: "foo(bar_" "baz) qux"
        let rows = |r| s.abs_row(r);
        let w = Selection::start(Kind::Word, pt(0, 5), &rows);
        assert_eq!(text_of(&s, &w), "bar_");
        let w = Selection::start(Kind::Word, pt(0, 3), &rows);
        assert_eq!(text_of(&s, &w), "(");
        let l = Selection::start(Kind::Line, pt(1, 2), &rows);
        assert_eq!(
            text_of(&s, &l),
            "foo(bar_baz) qux",
            "soft wrap joins without a newline"
        );
        assert_eq!(l.range(), (pt(0, 0), pt(1, 7)));
    }

    #[test]
    fn block_selects_a_rectangle() {
        let s = session("abcdef\r\nghijkl\r\nmnopqr", 10);
        let rows = |r| s.abs_row(r);
        let mut sel = Selection::start(Kind::Block, pt(0, 4), &rows);
        sel.update(pt(2, 1), &rows);
        assert_eq!(text_of(&s, &sel), "bcde\nhijk\nnopq");
        assert!(sel.contains(1, 2) && !sel.contains(1, 5) && !sel.contains(3, 2));
    }

    #[test]
    fn wide_chars_select_whole_and_extract_once() {
        let s = session("a日本b", 10);
        let rows = |r| s.abs_row(r);
        // Pointing at the spacer half of 日 (col 2) selects the whole glyph.
        let mut sel = Selection::start(Kind::Char, pt(0, 2), &rows);
        sel.update(pt(0, 3), &rows);
        assert_eq!(text_of(&s, &sel), "日本");
        assert_eq!(sel.range(), (pt(0, 1), pt(0, 4)));
    }

    #[test]
    fn output_over_selected_rows_invalidates() {
        let s = session("one\r\ntwo\r\nthree", 10);
        let rows = |r| s.abs_row(r);
        let sel = Selection::start(Kind::Char, pt(1, 0), &rows);
        let h = s.history_len();
        assert!(!invalidated(&sel, &Damage::Rows(vec![0, 2]), h, false));
        assert!(invalidated(&sel, &Damage::Rows(vec![1]), h, false));
        assert!(invalidated(&sel, &Damage::Full, h, false));
        assert!(invalidated(&sel, &Damage::Rows(vec![]), h, true));
        assert_eq!(
            merge_damage(Damage::Rows(vec![2]), Damage::Rows(vec![0, 2])),
            Damage::Rows(vec![0, 2])
        );
    }

    #[test]
    fn multi_click_counting() {
        let mut c = Clicks::default();
        let t = Instant::now();
        let w = Duration::from_millis(400);
        assert_eq!(c.press(t, pt(1, 1), w), 1);
        assert_eq!(c.press(t + Duration::from_millis(100), pt(1, 1), w), 2);
        assert_eq!(c.press(t + Duration::from_millis(200), pt(1, 1), w), 3);
        assert_eq!(c.press(t + Duration::from_millis(300), pt(1, 1), w), 1);
        assert_eq!(c.press(t + Duration::from_millis(350), pt(2, 1), w), 1);
        assert_eq!(c.press(t + Duration::from_millis(900), pt(2, 1), w), 1);
    }
}

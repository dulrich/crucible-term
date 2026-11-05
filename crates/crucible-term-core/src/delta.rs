//! The cell-level damage delta: the visible-screen cells that differ between a baseline
//! [`Grid`] and a newer one, plus the cursor. [`Delta::apply`] turns a copy of the baseline into
//! the newer screen; [`Delta::to_vt`] encodes the same change as VT bytes for a peer that holds
//! a [`crate::Session`] restored from the baseline. Pure: no I/O, no VT-core types.
//!
//! # Fidelity boundary
//!
//! **Carried:** the screen dimensions; per changed cell the text (incl. wide and combining),
//! foreground and background (default, indexed, RGB), bold, italic, faint, underline style,
//! inverse and strikethrough; the cursor position. [`Delta::apply`] (not `to_vt`) also carries
//! each changed row's soft-wrap flag and OSC 133 prompt mark.
//!
//! **Not carried:** scrollback (rows that scrolled off are not in the delta; resync scrollback
//! with [`crate::snapshot`]), terminal modes (DEC private modes, mouse and keyboard protocol
//! state), cursor pending-wrap, tab stops, scroll margins, palette changes, images, hyperlinks,
//! underline colour, blink, hidden. `to_vt` needs equal baseline and target dimensions
//! ([`Delta::resized`] false); a resize is carried only by `apply`.

use crate::model::{Cell, Grid, PromptMark, Row, Width};
use crate::snapshot::emit_cells;
use std::fmt::Write;

/// One changed cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellChange {
    /// Screen row.
    pub row: usize,
    /// Column.
    pub col: usize,
    /// The new content.
    pub cell: Cell,
}

/// Row flags of one changed row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowChange {
    /// Screen row.
    pub row: usize,
    /// New soft-wrap flag.
    pub wrapped: bool,
    /// New OSC 133 mark.
    pub prompt: PromptMark,
}

/// Changes from a baseline screen to a newer one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delta {
    /// Target columns.
    pub cols: usize,
    /// Target rows.
    pub rows: usize,
    /// Target cursor `(col, row)`.
    pub cursor: (usize, usize),
    /// True when the dimensions differ from the baseline (every cell is then listed).
    pub resized: bool,
    /// Changed cells, row-major.
    pub cells: Vec<CellChange>,
    /// Rows whose flags changed.
    pub row_flags: Vec<RowChange>,
}

impl Delta {
    /// The delta that turns `base`'s screen into `next`'s.
    pub fn between(base: &Grid, next: &Grid) -> Delta {
        let resized = base.cols != next.cols || base.rows != next.rows;
        let mut cells = Vec::new();
        let mut row_flags = Vec::new();
        for (y, row) in next.screen.iter().enumerate() {
            let old = if resized { None } else { base.screen.get(y) };
            if old.is_none_or(|o| o.wrapped != row.wrapped || o.prompt != row.prompt) {
                row_flags.push(RowChange {
                    row: y,
                    wrapped: row.wrapped,
                    prompt: row.prompt,
                });
            }
            for (x, cell) in row.cells.iter().enumerate() {
                if old.and_then(|o| o.cells.get(x)) != Some(cell) {
                    cells.push(CellChange {
                        row: y,
                        col: x,
                        cell: cell.clone(),
                    });
                }
            }
        }
        Delta {
            cols: next.cols,
            rows: next.rows,
            cursor: next.cursor,
            resized,
            cells,
            row_flags,
        }
    }

    /// True when nothing changed (cursor included).
    pub fn is_empty(&self, base: &Grid) -> bool {
        !self.resized
            && self.cells.is_empty()
            && self.row_flags.is_empty()
            && self.cursor == base.cursor
    }

    /// Apply to a copy of the baseline grid. Scrollback is left untouched.
    pub fn apply(&self, grid: &mut Grid) {
        if self.resized || grid.cols != self.cols || grid.rows != self.rows {
            grid.cols = self.cols;
            grid.rows = self.rows;
            grid.screen = vec![
                Row {
                    cells: vec![Cell::default(); self.cols],
                    ..Row::default()
                };
                self.rows
            ];
        }
        for c in &self.cells {
            grid.screen[c.row].cells[c.col] = c.cell.clone();
        }
        for r in &self.row_flags {
            grid.screen[r.row].wrapped = r.wrapped;
            grid.screen[r.row].prompt = r.prompt;
        }
        grid.cursor = self.cursor;
    }

    /// Encode as VT bytes: per run of adjacent changed cells a CUP and the cells, then the
    /// cursor. Only valid when `resized` is false.
    pub fn to_vt(&self) -> Vec<u8> {
        let mut o = String::new();
        let mut i = 0;
        while i < self.cells.len() {
            let start = &self.cells[i];
            let mut run = vec![start.cell.clone()];
            let mut next_col = start.col + span(&start.cell);
            let mut j = i + 1;
            while j < self.cells.len() {
                let c = &self.cells[j];
                if c.row != start.row {
                    break;
                }
                if c.cell.width == Width::Spacer && c.col + 1 == next_col {
                    j += 1;
                    continue;
                }
                if c.col != next_col {
                    break;
                }
                next_col += span(&c.cell);
                run.push(c.cell.clone());
                j += 1;
            }
            if !(run.len() == 1 && run[0].width == Width::Spacer) {
                let _ = write!(o, "\x1b[{};{}H", start.row + 1, start.col + 1);
                emit_cells(&mut o, &run);
            }
            i = j;
        }
        let _ = write!(o, "\x1b[{};{}H", self.cursor.1 + 1, self.cursor.0 + 1);
        o.into_bytes()
    }
}

fn span(c: &Cell) -> usize {
    if c.width == Width::Wide { 2 } else { 1 }
}

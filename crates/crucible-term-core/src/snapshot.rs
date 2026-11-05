//! The generic VT snapshot: a [`Grid`] re-emitted as a VT byte stream that, fed to a fresh
//! [`crate::Session`] of the same size and at least as much scrollback, reproduces the grid.
//! Pure: no I/O, no VT-core types.
//!
//! # Fidelity boundary
//!
//! **Carried:** every scrollback and screen row in order; per cell the text (base character
//! plus combining marks, wide glyphs with their spacer), foreground and background (default,
//! indexed 0-255, RGB), bold, italic, faint, underline style (single, double, curly, dotted,
//! dashed), inverse and strikethrough; per row the soft-wrap flag and the OSC 133 prompt mark;
//! the cursor position.
//!
//! **Not carried:** terminal modes (DEC private modes such as DECAWM, DECOM, alt screen,
//! bracketed paste, mouse and keyboard protocol state), the pending-wrap state of the cursor,
//! the SGR pen (the stream ends with SGR 0), tab stops, scroll margins, palette changes,
//! images, hyperlinks (OSC 8), underline colour, blink, hidden, title and cwd. Only the active
//! screen is carried: a session in the alternate screen snapshots the alternate screen as if
//! it were the primary.

use crate::model::{Attrs, Cell, Color, Grid, PromptMark, Row, Underline, Width};
use std::fmt::Write;

/// Encode `grid` as a VT byte stream (see the module docs for what it carries).
pub fn encode(grid: &Grid) -> Vec<u8> {
    // No ED 2: rio-vt pushes a cleared screen into scrollback; the target is a fresh session.
    let mut o = String::from("\x1b[0m\x1b[H");
    let rows: Vec<&Row> = grid.history.iter().chain(grid.screen.iter()).collect();
    let mut prev_wrapped = false;
    for (i, row) in rows.iter().enumerate() {
        match row.prompt {
            PromptMark::None => {}
            PromptMark::Prompt => o.push_str("\x1b]133;A\x07"),
            PromptMark::Continuation => o.push_str("\x1b]133;P;k=c\x07"),
        }
        let end = if row.wrapped {
            row.cells.len()
        } else {
            let last = row
                .cells
                .iter()
                .rposition(|c| !c.is_default())
                .map_or(0, |p| p + 1);
            // A continuation row must print at least one cell so the wrap is re-created.
            if prev_wrapped { last.max(1) } else { last }
        };
        emit_cells(&mut o, &row.cells[..end.min(row.cells.len())]);
        if i + 1 < rows.len() && !row.wrapped {
            o.push_str("\r\n");
        }
        prev_wrapped = row.wrapped;
    }
    let _ = write!(o, "\x1b[{};{}H", grid.cursor.1 + 1, grid.cursor.0 + 1);
    o.into_bytes()
}

/// Print `cells` from the current cursor position, skipping wide spacers. Ends with SGR 0
/// when any attribute was set.
pub(crate) fn emit_cells(o: &mut String, cells: &[Cell]) {
    let mut pen = Attrs::default();
    for c in cells {
        if c.width == Width::Spacer {
            continue;
        }
        if c.attrs != pen {
            sgr(o, &c.attrs);
            pen = c.attrs;
        }
        if c.text.is_empty() {
            o.push(' ');
        } else {
            o.push_str(&c.text);
        }
    }
    if pen != Attrs::default() {
        o.push_str("\x1b[0m");
    }
}

/// A full SGR that sets exactly `a` (starting from reset).
pub(crate) fn sgr(o: &mut String, a: &Attrs) {
    o.push_str("\x1b[0");
    if a.bold {
        o.push_str(";1");
    }
    if a.faint {
        o.push_str(";2");
    }
    if a.italic {
        o.push_str(";3");
    }
    let ul = match a.underline {
        Underline::None => 0,
        Underline::Single => 1,
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
    };
    if ul > 0 {
        let _ = write!(o, ";4:{ul}");
    }
    if a.inverse {
        o.push_str(";7");
    }
    if a.strikethrough {
        o.push_str(";9");
    }
    color(o, a.fg, 30, 90, 38);
    color(o, a.bg, 40, 100, 48);
    o.push('m');
}

fn color(o: &mut String, c: Color, base: u8, bright: u8, ext: u8) {
    let _ = match c {
        Color::Default => Ok(()),
        Color::Indexed(i) if i < 8 => write!(o, ";{}", base + i),
        Color::Indexed(i) if i < 16 => write!(o, ";{}", bright + i - 8),
        Color::Indexed(i) => write!(o, ";{ext};5;{i}"),
        Color::Rgb(r, g, b) => write!(o, ";{ext};2;{r};{g};{b}"),
    };
}

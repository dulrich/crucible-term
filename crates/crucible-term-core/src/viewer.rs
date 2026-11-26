//! The viewer frame encoder: what one viewer's terminal currently shows, and the VT bytes that
//! bring it to a session's current screen clipped to the viewer's size. Pure: no I/O, no clock,
//! no VT-core types; it works on [`Grid`], [`TermModes`] and [`CursorStyle`].
//!
//! # Use
//!
//! Send [`Viewer::enter`] once at attach, then per update [`Viewer::frame`] and, once the
//! frame's bytes have really been delivered, [`Viewer::commit`]. Send [`Viewer::leave`] at
//! detach.
//!
//! **Producing a frame does not advance the baseline.** [`Viewer::frame`] takes `&self`; only
//! [`Viewer::commit`] changes what the next frame is diffed against. A frame that is dropped
//! (never written) is simply not committed, and the next frame is computed from what the
//! terminal really shows. Frames depend only on the last committed frame and the current
//! inputs, never on intermediate frames, so a slow viewer skips frames. A frame that was only
//! partly written leaves the terminal in an unknown state; call [`Viewer::invalidate`] then.
//! Nothing changed gives an empty frame ([`Frame::is_empty`]): an idle session sends no bytes.
//!
//! A full repaint happens on the first frame, after [`Viewer::resize`] or
//! [`Viewer::invalidate`], and when the session's size differs from the last committed frame.
//!
//! # Clipping
//!
//! The viewer shows the top-left `cols x rows` of the session's screen. A wide glyph whose
//! spacer falls outside the right edge is shown blank. Where the viewer is larger than the
//! session the area outside the session's grid is blank with default attributes. The cursor is
//! hidden while it lies outside the clip.
//!
//! # Scrolling
//!
//! Each frame hashes every clipped row (one pass over the cells) and compares the hashes of the
//! baseline rows shifted up by `k` against the new rows, for every `k` (rows² hash compares, no
//! cell compare). The `k` that matches the most rows, if it beats no shift, is sent as a scroll
//! of the host terminal (`CSI k S`, inside `DECSTBM` limited to the session's rows when the
//! viewer is taller than the session) and the remaining rows are patched against the shifted
//! baseline cell by cell. The patch is exact whatever `k` was picked, so a wrong guess costs
//! bytes, never correctness. Only whole-clip scrolls are detected; a scroll region inside the
//! screen (a status line held still) is patched as changed rows.
//!
//! # Modes
//!
//! Mirrored on the host terminal, sending only transitions: cursor visibility (DECTCEM) and
//! shape (DECSCUSR, with blink), DECCKM, DECKPAM/DECKPNM, bracketed paste (2004), focus
//! reporting (1004), mouse reporting Off / 1000 / 1002 / 1003 (the previous mode is reset
//! before a new one is set) with default or SGR (1006) encoding, and the kitty keyboard flags
//! at the stack top. Not mirrored: the alternate screen (the viewer always sits on the host's
//! alternate screen) and alternate scroll (1007). Known limits of the seam: X10 mode 9 shows as
//! 1000 and the 1005/1015 encodings as the default encoding.
//!
//! Kitty keyboard: [`Viewer::enter`] pushes a fresh entry (`CSI > 0 u`) on the host's stack,
//! frames set that entry with `CSI = flags ; 1 u`, and [`Viewer::leave`] pops it (`CSI < u`),
//! so the host's own flags come back exactly, whatever they were. The kitty protocol keeps one
//! stack per screen; push and pop both happen on the alternate screen.
//!
//! # Sequences
//!
//! Enter: `CSI ? 1049 h`, `CSI > 0 u`, `CSI 0 m`, `CSI H`, `CSI 2 J`. Leave: `CSI 0 m`,
//! `CSI r`, the reset of every mirrored mode this viewer set (`CSI 0 SP q` first if it set a
//! cursor shape), `CSI ? 25 h`, `CSI < u`, `CSI ? 1049 l`. Only xterm-compatible sequences; no
//! terminfo.

use crate::input::{MouseEncoding, MouseMode, TermModes};
use crate::model::{Cell, CursorKind, CursorStyle, Grid, Width};
use crate::snapshot::emit_cells;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write;
use std::hash::{Hash, Hasher};

/// One viewer: its size, what its terminal shows (the last committed frame) and the host
/// modes it set.
#[derive(Clone, Debug)]
pub struct Viewer {
    cols: usize,
    rows: usize,
    shown: Option<Shown>,
    modes: Mirrored,
}

/// The bytes that bring a viewer from its last committed frame to the current screen, plus the
/// state the viewer is in once they are delivered.
#[derive(Clone, Debug)]
pub struct Frame {
    bytes: Vec<u8>,
    shown: Shown,
    modes: Mirrored,
}

impl Frame {
    /// The VT bytes to write to the viewer's terminal.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// True when the frame carries no bytes (nothing changed).
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// The clipped screen as the viewer's terminal shows it.
#[derive(Clone, Debug)]
struct Shown {
    /// The session's size the frame was taken from.
    source: (usize, usize),
    /// Exactly `rows` rows of exactly `cols` cells.
    rows: Vec<Vec<Cell>>,
    hashes: Vec<u64>,
    /// Cursor position, `None` when outside the clip.
    cursor: Option<(usize, usize)>,
}

/// Host-terminal modes as this viewer last set them. The default is the host's state after
/// [`Viewer::enter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mirrored {
    visible: bool,
    /// `None`: the host's default shape (never set).
    shape: Option<(CursorKind, bool)>,
    app_cursor: bool,
    app_keypad: bool,
    kitty: u8,
    mouse: MouseMode,
    encoding: MouseEncoding,
    paste: bool,
    focus: bool,
}

impl Default for Mirrored {
    fn default() -> Self {
        Mirrored {
            visible: true,
            shape: None,
            app_cursor: false,
            app_keypad: false,
            kitty: 0,
            mouse: MouseMode::Off,
            encoding: MouseEncoding::Default,
            paste: false,
            focus: false,
        }
    }
}

impl Viewer {
    /// A viewer of `cols x rows` that has shown nothing yet (the first frame repaints).
    pub fn new(cols: usize, rows: usize) -> Viewer {
        Viewer {
            cols,
            rows,
            shown: None,
            modes: Mirrored::default(),
        }
    }

    /// The viewer's size `(cols, rows)`.
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// The viewer's terminal changed size: the next frame repaints. Modes are kept.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.shown = None;
    }

    /// Forget what the terminal shows (for example after a partly written frame): the next frame
    /// repaints. Modes are kept as last committed.
    pub fn invalidate(&mut self) {
        self.shown = None;
    }

    /// The enter sequence, sent once at attach: alternate screen on, a fresh kitty keyboard
    /// entry pushed, attributes reset, screen cleared.
    pub fn enter() -> Vec<u8> {
        b"\x1b[?1049h\x1b[>0u\x1b[0m\x1b[H\x1b[2J".to_vec()
    }

    /// The leave sequence, sent at detach: restores the host terminal from whatever this viewer
    /// last committed (see the module docs). Valid with no frame committed; assumes
    /// [`Viewer::enter`] was sent.
    pub fn leave(&self) -> Vec<u8> {
        let mut o = String::from("\x1b[0m\x1b[r");
        // Visibility is forced on below; the kitty entry is popped, not reset.
        let host = Mirrored {
            visible: self.modes.visible,
            kitty: self.modes.kitty,
            ..Mirrored::default()
        };
        modes(&mut o, &self.modes, &host);
        o.push_str("\x1b[?25h\x1b[<u\x1b[?1049l");
        o.into_bytes()
    }

    /// A state-independent reset for a host terminal whose viewer state is unknown (the daemon
    /// was lost, or the transport to a remote viewer went away): SGR and scroll region reset,
    /// cursor visible in the default shape, DECCKM off, DECKPNM, bracketed paste, focus and
    /// every mouse mode off, the kitty keyboard entry popped (while still on the alternate
    /// screen, whose stack [`Viewer::enter`] pushed onto), then the alternate screen off.
    pub fn leave_all() -> Vec<u8> {
        b"\x1b[0m\x1b[r\x1b[0 q\x1b[?25h\x1b[?1l\x1b>\x1b[?2004l\x1b[?1004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\x1b[?1049l"
            .to_vec()
    }

    /// The frame that brings this viewer from its last committed state to `screen` (the
    /// session's visible screen, as [`crate::Session::screen`] returns it), `modes` and `style`.
    pub fn frame(&self, screen: &Grid, modes_now: &TermModes, style: CursorStyle) -> Frame {
        let shown = self.clip(screen);
        let mut o = String::new();
        let painted = match &self.shown {
            Some(old) if old.source == shown.source => patch(&mut o, old, &shown),
            _ => {
                repaint(&mut o, &shown);
                true
            }
        };
        let old_cursor = self.shown.as_ref().and_then(|s| s.cursor);
        if let Some((x, y)) = shown.cursor
            && (painted || old_cursor != shown.cursor)
        {
            let _ = write!(o, "\x1b[{};{}H", y + 1, x + 1);
        }
        let mut next = self.modes;
        next.visible = style.shape != CursorKind::Hidden && shown.cursor.is_some();
        if style.shape != CursorKind::Hidden {
            next.shape = Some((style.shape, style.blinking));
        }
        next.app_cursor = modes_now.app_cursor;
        next.app_keypad = modes_now.app_keypad;
        next.kitty = modes_now.kitty_flags;
        next.mouse = modes_now.mouse;
        next.encoding = modes_now.mouse_encoding;
        next.paste = modes_now.bracketed_paste;
        next.focus = modes_now.focus_events;
        modes(&mut o, &self.modes, &next);
        Frame {
            bytes: o.into_bytes(),
            shown,
            modes: next,
        }
    }

    /// Record that `frame`'s bytes were delivered: it becomes the baseline of the next frame.
    pub fn commit(&mut self, frame: Frame) {
        self.shown = Some(frame.shown);
        self.modes = frame.modes;
    }

    fn clip(&self, g: &Grid) -> Shown {
        let rows: Vec<Vec<Cell>> = (0..self.rows)
            .map(|y| {
                (0..self.cols)
                    .map(|x| {
                        match g.screen.get(y).and_then(|r| r.cells.get(x)) {
                            Some(c) if x < g.cols => {
                                // A wide glyph whose spacer is cut by the right edge is blank.
                                if c.width == Width::Wide && x + 1 >= self.cols {
                                    Cell::default()
                                } else {
                                    c.clone()
                                }
                            }
                            _ => Cell::default(),
                        }
                    })
                    .collect()
            })
            .collect();
        let hashes = rows.iter().map(|r| row_hash(r)).collect();
        let (cx, cy) = g.cursor;
        let inside = cx < self.cols.min(g.cols) && cy < self.rows.min(g.rows);
        Shown {
            source: (g.cols, g.rows),
            rows,
            hashes,
            cursor: inside.then_some((cx, cy)),
        }
    }
}

fn row_hash(cells: &[Cell]) -> u64 {
    let mut h = DefaultHasher::new();
    cells.hash(&mut h);
    h.finish()
}

/// Clear and paint every row.
fn repaint(o: &mut String, s: &Shown) {
    o.push_str("\x1b[0m\x1b[H\x1b[2J");
    for (y, row) in s.rows.iter().enumerate() {
        let tail = tail(row);
        if tail > 0 {
            let _ = write!(o, "\x1b[{};1H", y + 1);
            emit_cells(o, &row[..tail]);
        }
    }
}

/// One past the last non-default cell.
fn tail(row: &[Cell]) -> usize {
    row.iter()
        .rposition(|c| !c.is_default())
        .map_or(0, |p| p + 1)
}

/// Scroll if that helps, then patch changed rows. Returns whether anything was emitted.
fn patch(o: &mut String, old: &Shown, new: &Shown) -> bool {
    // Scroll detection works on the rows the session covers.
    let h = old.rows.len().min(new.source.1);
    let score = |k: usize| {
        (0..h - k)
            .filter(|&i| new.hashes[i] == old.hashes[i + k])
            .count()
    };
    let base = score(0);
    let mut best = (0, base);
    if base < h {
        for k in 1..h {
            let s = score(k);
            if s > best.1 {
                best = (k, s);
            }
        }
    }
    let k = best.0;
    let blank = vec![Cell::default(); new.rows.first().map_or(0, Vec::len)];
    let start = o.len();
    if k > 0 {
        if h < old.rows.len() {
            let _ = write!(o, "\x1b[1;{h}r\x1b[{k}S\x1b[r");
        } else {
            let _ = write!(o, "\x1b[{k}S");
        }
    }
    for (y, row) in new.rows.iter().enumerate() {
        let before: &[Cell] = if k == 0 || y >= h {
            &old.rows[y]
        } else if y + k < h {
            &old.rows[y + k]
        } else {
            &blank
        };
        let hb = if k == 0 || y >= h {
            old.hashes[y]
        } else if y + k < h {
            old.hashes[y + k]
        } else {
            row_hash(&blank)
        };
        if hb == new.hashes[y] && before == row.as_slice() {
            continue;
        }
        patch_row(o, y, before, row);
    }
    o.len() > start
}

/// Rewrite the cells of row `y` that differ between `old` and `new`.
fn patch_row(o: &mut String, y: usize, old: &[Cell], new: &[Cell]) {
    let Some(mut x0) = (0..new.len()).find(|&x| old[x] != new[x]) else {
        return;
    };
    let x1 = (0..new.len()).rfind(|&x| old[x] != new[x]).unwrap_or(x0);
    if new[x0].width == Width::Spacer && x0 > 0 {
        x0 -= 1;
    }
    let tail = tail(new);
    let _ = write!(o, "\x1b[{};{}H", y + 1, x0 + 1);
    if x1 >= tail {
        if x0 < tail {
            emit_cells(o, &new[x0..tail]);
        }
        o.push_str("\x1b[K");
    } else {
        let end = if new[x1].width == Width::Wide {
            x1 + 2
        } else {
            x1 + 1
        };
        emit_cells(o, &new[x0..end.min(new.len())]);
    }
}

/// Emit the transitions from `from` to `to`.
fn modes(o: &mut String, from: &Mirrored, to: &Mirrored) {
    let flag = |o: &mut String, a: bool, b: bool, n: u16| {
        if a != b {
            let _ = write!(o, "\x1b[?{n}{}", if b { 'h' } else { 'l' });
        }
    };
    if from.shape != to.shape {
        let n = match to.shape {
            None => 0,
            Some((CursorKind::Underline, true)) => 3,
            Some((CursorKind::Underline, false)) => 4,
            Some((CursorKind::Beam, true)) => 5,
            Some((CursorKind::Beam, false)) => 6,
            Some((_, true)) => 1,
            Some((_, false)) => 2,
        };
        let _ = write!(o, "\x1b[{n} q");
    }
    flag(o, from.visible, to.visible, 25);
    flag(o, from.app_cursor, to.app_cursor, 1);
    if from.app_keypad != to.app_keypad {
        o.push_str(if to.app_keypad { "\x1b=" } else { "\x1b>" });
    }
    flag(o, from.paste, to.paste, 2004);
    flag(o, from.focus, to.focus, 1004);
    if from.mouse != to.mouse {
        if let Some(n) = mouse_code(from.mouse) {
            let _ = write!(o, "\x1b[?{n}l");
        }
        if let Some(n) = mouse_code(to.mouse) {
            let _ = write!(o, "\x1b[?{n}h");
        }
    }
    flag(
        o,
        from.encoding == MouseEncoding::Sgr,
        to.encoding == MouseEncoding::Sgr,
        1006,
    );
    if from.kitty != to.kitty {
        let _ = write!(o, "\x1b[={};1u", to.kitty);
    }
}

fn mouse_code(m: MouseMode) -> Option<u16> {
    match m {
        MouseMode::Off => None,
        MouseMode::Click => Some(1000),
        MouseMode::Drag => Some(1002),
        MouseMode::Any => Some(1003),
    }
}

#[cfg(test)]
mod tests {
    use super::Viewer;

    #[test]
    fn leave_all_is_the_fixed_reset() {
        assert_eq!(
            Viewer::leave_all(),
            b"\x1b[0m\x1b[r\x1b[0 q\x1b[?25h\x1b[?1l\x1b>\x1b[?2004l\x1b[?1004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\x1b[?1049l"
        );
    }
}

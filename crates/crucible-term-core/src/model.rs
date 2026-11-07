//! CDE-owned terminal model types. Nothing here names a rio-vt type, so the renderer, the
//! snapshot and the delta stay independent of the VT core behind [`crate::Session`].

use std::path::PathBuf;

/// A cell colour. Indices 0-15 are the ANSI/bright palette, 16-255 the xterm cube and ramp.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    /// The default foreground or background (whichever slot the colour sits in).
    #[default]
    Default,
    /// A palette index.
    Indexed(u8),
    /// A direct (truecolor) value.
    Rgb(u8, u8, u8),
}

/// Underline style (SGR 4 and its `4:n` sub-parameters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Underline {
    /// No underline.
    #[default]
    None,
    /// `4` / `4:1`.
    Single,
    /// `21` / `4:2`.
    Double,
    /// `4:3`.
    Curly,
    /// `4:4`.
    Dotted,
    /// `4:5`.
    Dashed,
}

/// Rendition attributes of one cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs {
    /// Foreground colour.
    pub fg: Color,
    /// Background colour.
    pub bg: Color,
    /// SGR 1.
    pub bold: bool,
    /// SGR 3.
    pub italic: bool,
    /// SGR 2 (dim).
    pub faint: bool,
    /// SGR 7.
    pub inverse: bool,
    /// SGR 9.
    pub strikethrough: bool,
    /// SGR 4 family.
    pub underline: Underline,
}

/// How many columns a cell's glyph covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Width {
    /// A one-column glyph (or a blank).
    #[default]
    Narrow,
    /// The first column of a two-column glyph.
    Wide,
    /// The second column of a two-column glyph; carries no text.
    Spacer,
}

/// One grid cell.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Cell {
    /// The grapheme: a base character plus any combining marks. Empty for a blank cell
    /// (a cleared cell and a typed space are both blank) and for a wide-glyph spacer.
    pub text: String,
    /// Glyph width class.
    pub width: Width,
    /// Rendition.
    pub attrs: Attrs,
}

impl Cell {
    /// True for a blank cell with default attributes.
    pub fn is_default(&self) -> bool {
        self.text.is_empty() && self.width == Width::Narrow && self.attrs == Attrs::default()
    }
}

/// OSC 133 mark on a row, as recorded by the VT core (`A` = prompt start; `P;k=c|s` =
/// continuation). The core records marks per row; `B`/`C`/`D` are not recorded (see crate docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PromptMark {
    /// No mark.
    #[default]
    None,
    /// The row holds the start of a shell prompt.
    Prompt,
    /// The row holds a prompt continuation.
    Continuation,
}

/// One grid row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Row {
    /// Exactly `cols` cells.
    pub cells: Vec<Cell>,
    /// The row soft-wraps into the next one (it is part of a longer logical line).
    pub wrapped: bool,
    /// OSC 133 mark.
    pub prompt: PromptMark,
}

impl Row {
    /// The row's text with spacers skipped, blanks as spaces and trailing blanks trimmed.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.cells {
            match (c.width, c.text.is_empty()) {
                (Width::Spacer, _) => {}
                (_, true) => s.push(' '),
                (_, false) => s.push_str(&c.text),
            }
        }
        s.truncate(s.trim_end_matches(' ').len());
        s
    }
}

/// A copy of the active screen plus scrollback, in CDE-owned types.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grid {
    /// Columns.
    pub cols: usize,
    /// Visible rows.
    pub rows: usize,
    /// Scrollback rows, oldest first.
    pub history: Vec<Row>,
    /// Visible rows, top first; exactly `rows` entries.
    pub screen: Vec<Row>,
    /// Cursor `(col, row)` on the visible screen, 0-based.
    pub cursor: (usize, usize),
}

/// Something the session reports to its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// OSC 0/2 set the title (empty string: reset to the default).
    Title(String),
    /// OSC 7 reported a new working directory.
    Cwd(PathBuf),
    /// OSC 52 asked to store `text` in the selection named by `primary` (false: clipboard).
    /// Policy (allow, deny, prompt) belongs to the caller.
    ClipboardStore {
        /// True for the primary selection (`p`/`s`), false for the clipboard (`c`).
        primary: bool,
        /// The decoded text.
        text: String,
    },
    /// OSC 52 asked to read the selection. Policy belongs to the caller (crucible-term's
    /// `clipboard::osc52_policy`), which never answers it.
    ClipboardLoad {
        /// True for the primary selection.
        primary: bool,
    },
    /// BEL.
    Bell,
}

/// What changed on the visible screen since the last [`crate::Session::take_damage`].
/// Line-level: rio-vt tracks damage per screen row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Damage {
    /// Redraw everything (resize, scroll, mode flips, full clears).
    Full,
    /// Redraw these visible rows (ascending, deduplicated; may be empty).
    Rows(Vec<usize>),
}

/// The cursor shape to draw (DECSCUSR), or hidden (DECTCEM off).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CursorKind {
    /// A full-cell block.
    #[default]
    Block,
    /// An underline bar.
    Underline,
    /// A vertical beam at the left edge.
    Beam,
    /// Not drawn.
    Hidden,
}

/// How the cursor should be drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CursorStyle {
    /// Shape, or hidden.
    pub shape: CursorKind,
    /// DECSCUSR asked for a blinking cursor.
    pub blinking: bool,
}

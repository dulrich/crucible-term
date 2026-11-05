//! The rio-vt adapter. This is the only module that names rio-vt types; everything it hands
//! out is a CDE-owned type from [`crate::model`].

use crate::ed1::Ed1Fix;
use crate::model::{
    Attrs, Cell, Color, CursorKind, CursorStyle, Damage, Event, Grid, PromptMark, Row, Underline,
    Width,
};
use rio_vt::ansi::CursorShape;
use rio_vt::clipboard::ClipboardType;
use rio_vt::config::colors::AnsiColor;
use rio_vt::crosswords::grid::Dimensions;
use rio_vt::crosswords::grid::row::SemanticPrompt;
use rio_vt::crosswords::pos::{Column, Line, Pos};
use rio_vt::crosswords::square::{CellFlags, Wide};
use rio_vt::crosswords::style::{StyleFlags, UnderlineKind};
use rio_vt::crosswords::{Crosswords, TermDamage};
use rio_vt::event::{EventListener, RioEvent, WindowId};
use rio_vt::performer::handler::Processor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// What the listener collects between two `feed` calls.
#[derive(Default)]
struct Sink {
    replies: Vec<u8>,
    events: Vec<Event>,
}

/// rio-vt's event listener: replies and events land in a shared sink.
#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Sink>>);

impl Listener {
    fn with<R>(&self, f: impl FnOnce(&mut Sink) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: RioEvent, _id: WindowId) {
        let primary = |t: &ClipboardType| matches!(t, ClipboardType::Selection);
        self.with(|s| match event {
            RioEvent::PtyWrite(_, text) => s.replies.extend_from_slice(text.as_bytes()),
            RioEvent::Title(_, title) => s.events.push(Event::Title(title)),
            RioEvent::ResetTitle => s.events.push(Event::Title(String::new())),
            // The event carries no path; `Session::feed` fills it from `current_directory`.
            RioEvent::CurrentDirectoryChanged(_) => s.events.push(Event::Cwd(PathBuf::new())),
            RioEvent::ClipboardStore(t, text) => s.events.push(Event::ClipboardStore {
                primary: primary(&t),
                text,
            }),
            RioEvent::ClipboardLoad(_, t, _) => s.events.push(Event::ClipboardLoad {
                primary: primary(&t),
            }),
            RioEvent::Bell(_) => s.events.push(Event::Bell),
            _ => {}
        });
    }
    fn send_event_with_high_priority(&self, event: RioEvent, id: WindowId) {
        self.send_event(event, id);
    }
    fn send_global_event(&self, event: RioEvent) {
        self.send_event(event, WindowId::from(0));
    }
}

#[derive(Clone, Copy)]
struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

/// One terminal session: VT state for one PTY. Feed it the child's output, hand its replies
/// back to the child, read the grid to render.
pub struct Session {
    term: Crosswords<Listener>,
    parser: Processor,
    sink: Listener,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (cols, rows) = self.size();
        f.debug_struct("Session")
            .field("cols", &cols)
            .field("rows", &rows)
            .field("history", &self.history_len())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// A blank session of `cols` x `rows` keeping up to `scrollback` rows of history.
    /// Zero dimensions are raised to 1.
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let sink = Listener::default();
        let term = Crosswords::new(
            Size(cols.max(1), rows.max(1)),
            CursorShape::Block,
            sink.clone(),
            WindowId::from(0),
            0,
            scrollback,
        );
        Session {
            term,
            parser: Processor::default(),
            sink,
        }
    }

    /// Parse PTY output bytes into the grid. Replies the child expects (DA, DSR, DECRQM,
    /// XTVERSION, ...) accumulate for [`Session::take_replies`]; events for
    /// [`Session::take_events`].
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut fix = Ed1Fix(&mut self.term);
        self.parser.advance(&mut fix, bytes);
        if let Some(dir) = self.term.current_directory.clone() {
            self.sink.with(|s| {
                for e in &mut s.events {
                    if let Event::Cwd(p) = e
                        && p.as_os_str().is_empty()
                    {
                        *p = dir.clone();
                    }
                }
            });
        }
    }

    /// Resize the screen, reflowing soft-wrapped lines. Zero dimensions are raised to 1.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(Size(cols.max(1), rows.max(1)));
    }

    /// `(cols, rows)` of the visible screen.
    pub fn size(&self) -> (usize, usize) {
        (self.term.grid.columns(), self.term.grid.screen_lines())
    }

    /// Cursor `(col, row)` on the visible screen.
    pub fn cursor(&self) -> (usize, usize) {
        let p = self.term.grid.cursor.pos;
        (p.col.0, usize::try_from(p.row.0).unwrap_or(0))
    }

    /// The cursor as the renderer should draw it: shape (DECSCUSR), `Hidden` under DECTCEM
    /// off, and whether DECSCUSR asked for blinking.
    pub fn cursor_style(&self) -> CursorStyle {
        let shape = match self.term.cursor().content {
            CursorShape::Block => CursorKind::Block,
            CursorShape::Underline => CursorKind::Underline,
            CursorShape::Beam => CursorKind::Beam,
            CursorShape::Hidden => CursorKind::Hidden,
        };
        CursorStyle {
            shape,
            blinking: self.term.blinking_cursor,
        }
    }

    /// True while the child holds a mode 2026 synchronized update (BSU without ESU yet).
    /// The window holds presenting while this is set, under its own timeout.
    pub fn synchronized_output(&self) -> bool {
        self.term.synchronized_output()
    }

    /// Rows of scrollback currently held.
    pub fn history_len(&self) -> usize {
        self.term.grid.total_lines() - self.term.grid.screen_lines()
    }

    /// The last OSC 7 working directory, if any.
    pub fn cwd(&self) -> Option<&Path> {
        self.term.current_directory.as_deref()
    }

    /// Row `y` of the visible screen (0 = top). Panics if `y >= rows`.
    pub fn screen_row(&self, y: usize) -> Row {
        self.row_at(Line(i32::try_from(y).unwrap_or(i32::MAX)))
    }

    /// Scrollback row `i` (0 = oldest). Panics if `i >= history_len()`.
    pub fn history_row(&self, i: usize) -> Row {
        let h = self.history_len();
        assert!(i < h, "history row {i} out of range ({h})");
        self.row_at(Line(-i32::try_from(h - i).unwrap_or(i32::MAX)))
    }

    /// A full copy of scrollback, screen and cursor.
    pub fn grid(&self) -> Grid {
        let (cols, rows) = self.size();
        Grid {
            cols,
            rows,
            history: (0..self.history_len())
                .map(|i| self.history_row(i))
                .collect(),
            screen: (0..rows).map(|y| self.screen_row(y)).collect(),
            cursor: self.cursor(),
        }
    }

    /// Only the visible screen and cursor (no scrollback) — cheap per-frame copy.
    pub fn screen(&self) -> Grid {
        let (cols, rows) = self.size();
        Grid {
            cols,
            rows,
            history: Vec::new(),
            screen: (0..rows).map(|y| self.screen_row(y)).collect(),
            cursor: self.cursor(),
        }
    }

    /// Visible-screen damage since the previous call (line-level), then reset.
    pub fn take_damage(&mut self) -> Damage {
        let d = match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(it) => {
                let mut rows: Vec<usize> = it.map(|l| l.line).collect();
                rows.sort_unstable();
                rows.dedup();
                Damage::Rows(rows)
            }
        };
        self.term.reset_damage();
        d
    }

    /// Bytes to write back to the child (terminal replies), drained.
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.sink.with(|s| std::mem::take(&mut s.replies))
    }

    /// Events since the previous call, in order, drained.
    pub fn take_events(&mut self) -> Vec<Event> {
        self.sink.with(|s| std::mem::take(&mut s.events))
    }

    /// The generic VT snapshot of this session (see [`crate::snapshot`]).
    pub fn snapshot(&self) -> Vec<u8> {
        crate::snapshot::encode(&self.grid())
    }

    fn row_at(&self, line: Line) -> Row {
        let g = &self.term.grid;
        let cols = g.columns();
        let cells = (0..cols)
            .map(|x| {
                let pos = Pos::new(line, Column(x));
                let sq = g[pos];
                let st = g.style_of(&sq);
                let width = match sq.wide() {
                    Wide::Wide => Width::Wide,
                    Wide::Spacer => Width::Spacer,
                    Wide::Narrow | Wide::LeadingSpacer => Width::Narrow,
                };
                let text = match sq.wide() {
                    Wide::Spacer | Wide::LeadingSpacer => String::new(),
                    _ => {
                        let t: String = g.cell_text(pos).filter(|c| *c != '\0').collect();
                        if t == " " { String::new() } else { t }
                    }
                };
                let f = st.flags;
                Cell {
                    text,
                    width,
                    attrs: Attrs {
                        fg: color(st.fg),
                        bg: color(st.bg),
                        bold: f.contains(StyleFlags::BOLD),
                        italic: f.contains(StyleFlags::ITALIC),
                        faint: f.contains(StyleFlags::DIM),
                        inverse: f.contains(StyleFlags::INVERSE),
                        strikethrough: f.contains(StyleFlags::STRIKEOUT),
                        underline: match f.underline_kind() {
                            None => Underline::None,
                            Some(UnderlineKind::Single) => Underline::Single,
                            Some(UnderlineKind::Double) => Underline::Double,
                            Some(UnderlineKind::Curly) => Underline::Curly,
                            Some(UnderlineKind::Dotted) => Underline::Dotted,
                            Some(UnderlineKind::Dashed) => Underline::Dashed,
                        },
                    },
                }
            })
            .collect();
        let wrapped =
            cols > 0 && g[Pos::new(line, Column(cols - 1))].contains_cell_flag(CellFlags::WRAPLINE);
        let prompt = match g[line].semantic_prompt {
            SemanticPrompt::None => PromptMark::None,
            SemanticPrompt::Prompt => PromptMark::Prompt,
            SemanticPrompt::PromptContinuation => PromptMark::Continuation,
        };
        Row {
            cells,
            wrapped,
            prompt,
        }
    }
}

fn color(c: AnsiColor) -> Color {
    match c {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => Color::Indexed(i),
        AnsiColor::Named(n) => match u8::try_from(n as usize) {
            Ok(i) if i < 16 => Color::Indexed(i),
            _ => Color::Default,
        },
    }
}

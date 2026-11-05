//! `crucible-term-core`: the CDE-owned seam between crucible-term and its VT core.
//!
//! [`Session`] wraps rio-vt (pinned `=0.5.29`) and hands out only CDE-owned types
//! ([`model`]): cells, rows, colours, attributes, events, damage. No rio-vt type is public,
//! so swapping the core (libghostty-vt is the ranked alternative) is a change to this crate
//! alone. On top of the grid sit two pure encodings for later attach work: the generic VT
//! [`snapshot`] and the cell-level [`delta`]. The [`pty`] module owns the pseudo-terminal.
//!
//! Adapted from rio-vt natively (no parser of our own): OSC 0/2 titles (`RioEvent::Title`),
//! OSC 7 (`set_current_directory` → `current_directory` + `CurrentDirectoryChanged`), OSC 133
//! (`set_semantic_prompt` → per-row `semantic_prompt`, exposed as [`model::PromptMark`]; rio-vt
//! records `A` and `P` only, so `B`/`C`/`D` command and output boundaries are not available),
//! OSC 52 (`ClipboardStore` / `ClipboardLoad`), BEL, and child-facing replies (`PtyWrite`).
//!
//! The ED 1 correction (rio-vt's inherited `CSI 1 J` bug) lives in a private `Handler`
//! wrapper; see `ed1.rs`.
//!
//! No Wayland or GUI dependency; snapshot and delta logic does no I/O.

pub mod delta;
mod ed1;
pub mod model;
pub mod pty;
mod session;
pub mod snapshot;

pub use delta::Delta;
pub use model::{
    Attrs, Cell, Color, CursorKind, CursorStyle, Damage, Event, Grid, PromptMark, Row, Underline,
    Width,
};
pub use session::Session;

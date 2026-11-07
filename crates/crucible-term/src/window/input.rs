//! Seat input for the window: `wl_keyboard` (xkbcommon keymap and repeat via sctk), `wl_pointer`,
//! the selection, and the clipboard and primary-selection devices. Everything decision-like is
//! a pure function elsewhere ([`crate::selection`], [`crate::clipboard`],
//! `crucible_term_core::input`); this module translates sctk events into those types and acts
//! on the results.
//!
//! Routing summary (foot's rules):
//! - Keys: `Ctrl+Shift+C/V` copy and paste, `Shift+PageUp/PageDown/Home/End` scroll the
//!   viewport; those chords are consumed. Everything else is encoded by `encode_key`, scrolls
//!   the viewport to the bottom and goes through the one PTY outbox.
//! - Pointer: when the child requested mouse reporting and Shift is NOT held, events are
//!   encoded and sent. **Shift always overrides reporting** and does local selection.
//! - Wheel: reporting and no Shift, report; else alternate screen with 1007, arrow keys (3 per
//!   notch); else scroll the scrollback viewport (3 lines per notch).

use super::host::Host;
use super::routing::TermId;
use super::term::Term;
use crate::clipboard::{self, MIME_TYPES, Osc52};
use crate::render;
use crate::selection::{Clicks, Kind, Point, Selection};
use crucible_term_core::{
    Key, Mods, MouseAction, MouseButton, MouseEvent, MouseMode, encode_key, encode_mouse,
};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, ReadPipe, WritePipe};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::{
    PrimarySelectionDevice, PrimarySelectionDeviceHandler,
};
use smithay_client_toolkit::primary_selection::selection::{
    PrimarySelectionSource, PrimarySelectionSourceHandler,
};
use smithay_client_toolkit::reexports::calloop::PostAction;
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::{Interest, Mode};
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler,
    ThemedPointer,
};
use smithay_client_toolkit::{
    delegate_data_device, delegate_keyboard, delegate_pointer, delegate_primary_selection,
};
use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};

/// Lines scrolled per wheel notch, and arrow keys sent per notch on the alternate screen.
const WHEEL_LINES: i32 = 3;
/// Two presses within this long on one cell count as a double (then triple) click.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// A paste larger than this is dropped (and logged) rather than held in memory.
const MAX_PASTE: usize = 64 << 20;

/// Seat-wide input, owned by the [`Host`]: devices, modifiers, the latest serial, and the
/// clipboard and primary-selection sources (one clipboard per seat, whichever window set it).
pub struct SeatInput {
    pub(super) keyboard: Option<wl_keyboard::WlKeyboard>,
    pub(super) pointer: Option<ThemedPointer>,
    /// Which window the pointer is in and the icon last set for it.
    pub(super) cursor: CursorState,
    pub(super) mods: Mods,
    /// The latest input serial, used for `set_selection`.
    serial: u32,
    data_mgr: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    primary_mgr: Option<PrimarySelectionManagerState>,
    primary_device: Option<PrimarySelectionDevice>,
    copy_source: Option<CopyPasteSource>,
    primary_source: Option<PrimarySelectionSource>,
    clip_text: Arc<str>,
    primary_text: Arc<str>,
    /// The connection the devices belong to (`None`: detached, every device gone).
    qh: Option<QueueHandle<Host>>,
}

/// The pointer-cursor bookkeeping: the window under the pointer (set on Enter, cleared on
/// Leave), the icon last set for it, and whether a `set_cursor` failure was already logged for
/// the current pointer binding.
#[derive(Default)]
pub(super) struct CursorState {
    pub(super) term: Option<TermId>,
    last: Option<CursorIcon>,
    err_logged: bool,
}

impl CursorState {
    /// The pointer left its window: forget the window and the cached icon.
    fn leave(&mut self) {
        self.term = None;
        self.last = None;
    }
}

/// The pointer icon for a window: the application's OSC 22 icon if set, else the arrow while
/// the child has mouse reporting on and Shift is not held (clicks go to the application), else
/// the I-beam.
pub(super) fn desired_icon(app: Option<CursorIcon>, reporting: bool, shift: bool) -> CursorIcon {
    if let Some(icon) = app {
        icon
    } else if reporting && !shift {
        CursorIcon::Default
    } else {
        CursorIcon::Text
    }
}

/// Per-window pointer gesture and selection state, owned by its [`Term`].
#[derive(Default)]
pub struct Input {
    /// Pointer position in logical surface coordinates.
    pos: (f64, f64),
    /// The button whose press was reported to the child (its release and drags follow).
    reported: Option<MouseButton>,
    pub(super) selection: Option<Selection>,
    /// A char/block selection that starts once the pointer leaves its first cell.
    pending: Option<(Kind, Point)>,
    selecting: bool,
    clicks: Clicks,
    wheel: f64,
}

impl SeatInput {
    pub(super) fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<Host>,
    ) -> SeatInput {
        SeatInput {
            keyboard: None,
            pointer: None,
            cursor: CursorState::default(),
            mods: Mods::default(),
            serial: 0,
            data_mgr: DataDeviceManagerState::bind(globals, qh).ok(),
            data_device: None,
            primary_mgr: PrimarySelectionManagerState::bind(globals, qh).ok(),
            primary_device: None,
            copy_source: None,
            primary_source: None,
            clip_text: Arc::from(""),
            primary_text: Arc::from(""),
            qh: Some(qh.clone()),
        }
    }

    /// Set the pointer cursor now. `force` sets even when `icon` equals the last one set (every
    /// Enter: the compositor may have shown another client's cursor meanwhile); otherwise a
    /// repeat is skipped. Errors are logged once per pointer binding and never fatal.
    pub(super) fn set_cursor(&mut self, icon: CursorIcon, force: bool) {
        if !force && self.cursor.last == Some(icon) {
            return;
        }
        let Some(p) = self.pointer.as_ref() else {
            return;
        };
        let Some(backend) = p.pointer().backend().upgrade() else {
            return;
        };
        let conn = Connection::from_backend(backend);
        self.cursor.last = Some(icon);
        if let Err(e) = p.set_cursor(&conn, icon)
            && !self.cursor.err_logged
        {
            self.cursor.err_logged = true;
            eprintln!("crucible-term: cursor: {e}");
        }
    }

    /// A new pointer binding: forget the window, the cached icon and the logged error.
    pub(super) fn bind_pointer(&mut self, p: ThemedPointer) {
        self.pointer = Some(p);
        self.cursor = CursorState::default();
    }

    /// The pointer device is gone: dropping the themed pointer destroys the shape device,
    /// releases the pointer and destroys the cursor surface.
    pub(super) fn unbind_pointer(&mut self) {
        self.pointer = None;
        self.cursor = CursorState::default();
    }

    /// No connection: no devices, no selection sources (the detached host's seat).
    pub(super) fn detached() -> SeatInput {
        SeatInput {
            keyboard: None,
            pointer: None,
            cursor: CursorState::default(),
            mods: Mods::default(),
            serial: 0,
            data_mgr: None,
            data_device: None,
            primary_mgr: None,
            primary_device: None,
            copy_source: None,
            primary_source: None,
            clip_text: Arc::from(""),
            primary_text: Arc::from(""),
            qh: None,
        }
    }

    /// Create the data and primary-selection devices for the first seat (missing globals: skip).
    pub(super) fn bind_devices(
        &mut self,
        qh: &QueueHandle<Host>,
        seat: &wayland_client::protocol::wl_seat::WlSeat,
    ) {
        if self.data_device.is_none() {
            self.data_device = self.data_mgr.as_ref().map(|m| m.get_data_device(qh, seat));
        }
        if self.primary_device.is_none() {
            self.primary_device = self
                .primary_mgr
                .as_ref()
                .map(|m| m.get_selection_device(qh, seat));
        }
    }

    fn set_clipboard(&mut self, text: String, serial: u32) {
        let (Some(mgr), Some(dev), Some(qh)) = (&self.data_mgr, &self.data_device, &self.qh) else {
            return;
        };
        let src = mgr.create_copy_paste_source(qh, MIME_TYPES);
        src.set_selection(dev, serial);
        self.clip_text = Arc::from(text);
        self.copy_source = Some(src);
    }

    fn set_primary(&mut self, text: String, serial: u32) {
        let (Some(mgr), Some(dev), Some(qh)) = (&self.primary_mgr, &self.primary_device, &self.qh)
        else {
            return;
        };
        let src = mgr.create_selection_source(qh, MIME_TYPES);
        src.set_selection(dev, serial);
        self.primary_text = Arc::from(text);
        self.primary_source = Some(src);
    }
}

/// Decode a keysym (and the compositor's composed text) into a core key. `None`: not encodable.
/// The bool is "this keysym implies Shift" (`ISO_Left_Tab`).
pub fn translate(sym: Keysym, utf8: Option<&str>) -> Option<(Key, bool)> {
    let key = match sym {
        Keysym::Return | Keysym::KP_Enter => Key::Enter,
        Keysym::Tab => Key::Tab,
        Keysym::ISO_Left_Tab => return Some((Key::Tab, true)),
        Keysym::BackSpace => Key::Backspace,
        Keysym::Escape => Key::Escape,
        Keysym::Up => Key::Up,
        Keysym::Down => Key::Down,
        Keysym::Left => Key::Left,
        Keysym::Right => Key::Right,
        Keysym::Home => Key::Home,
        Keysym::End => Key::End,
        Keysym::Page_Up => Key::PageUp,
        Keysym::Page_Down => Key::PageDown,
        Keysym::Insert => Key::Insert,
        Keysym::Delete => Key::Delete,
        s if (Keysym::F1.raw()..=Keysym::F12.raw()).contains(&s.raw()) => {
            Key::F((s.raw() - Keysym::F1.raw() + 1) as u8)
        }
        s => {
            // Composed text wins (dead keys); control characters (Ctrl held) fall back to the keysym.
            let mut it = utf8.unwrap_or("").chars().filter(|c| !c.is_control());
            let composed = it.next().filter(|_| it.next().is_none());
            Key::Char(composed.or_else(|| s.key_char().filter(|c| !c.is_control()))?)
        }
    };
    Some((key, false))
}

impl Term {
    // ---- PTY writes ---------------------------------------------------------------------

    /// Queue `bytes` for the child and write what the PTY accepts now. Every PTY write goes
    /// through here (replies, keys, paste).
    pub(super) fn send(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.outbox.push(bytes);
        if self.write_source.is_none() && !self.drain_outbox() {
            self.arm_writable();
        }
    }

    /// Flush the outbox; true when nothing is left to wait for (drained, or a hard error that
    /// dropped the queue).
    pub(super) fn drain_outbox(&mut self) -> bool {
        let mut master: &File = self.pty.master();
        match self.outbox.flush(&mut master) {
            Ok(done) => done,
            Err(e) => {
                eprintln!("crucible-term: pty write: {e}");
                self.outbox.clear();
                true
            }
        }
    }

    /// Wait for the master to become writable, then drain the rest of the outbox.
    fn arm_writable(&mut self) {
        let Ok(fd) = self.pty.master().try_clone() else {
            self.outbox.clear();
            return;
        };
        let id = self.id;
        let r = self.handle.insert_source(
            Generic::new(fd, Interest::WRITE, Mode::Level),
            move |_, _, host| {
                let Some(t) = host.terms.get_mut(&id) else {
                    return Ok(PostAction::Remove);
                };
                if t.drain_outbox() {
                    t.write_source = None;
                    Ok(PostAction::Remove)
                } else {
                    Ok(PostAction::Continue)
                }
            },
        );
        match r {
            Ok(token) => self.write_source = Some(token),
            Err(e) => {
                eprintln!("crucible-term: pty writable source: {e}");
                self.outbox.clear();
            }
        }
    }

    // ---- keyboard -----------------------------------------------------------------------

    fn on_key(&mut self, seat: &mut SeatInput, event: &KeyEvent, repeat: bool) {
        let Some((key, implied_shift)) = translate(event.keysym, event.utf8.as_deref()) else {
            return;
        };
        let mut mods = seat.mods;
        mods.shift |= implied_shift;
        let (ctrl_shift, shift_only) = (
            mods.ctrl && mods.shift && !mods.alt && !mods.logo,
            mods.shift && !mods.ctrl && !mods.alt && !mods.logo,
        );
        if ctrl_shift && let Key::Char(c) = key {
            match c.to_ascii_lowercase() {
                'c' => {
                    if !repeat {
                        self.copy_selection(seat);
                    }
                    return;
                }
                'v' => {
                    if !repeat {
                        self.paste_clipboard(seat);
                    }
                    return;
                }
                _ => {}
            }
        }
        if shift_only {
            match key {
                Key::PageUp => return self.scroll_viewport(|s| s.scroll_page(true)),
                Key::PageDown => return self.scroll_viewport(|s| s.scroll_page(false)),
                Key::Home => return self.scroll_viewport(|s| s.scroll_to_top()),
                Key::End => return self.scroll_viewport(|s| s.scroll_to_bottom()),
                _ => {}
            }
        }
        let modes = self.session.modes();
        let bytes = match encode_key(key, mods, &modes) {
            Some(b) => b,
            None => return,
        };
        self.snap_to_bottom();
        self.send(&bytes);
    }

    /// Run a viewport scroll and redraw (a changed offset is full damage in the core).
    fn scroll_viewport(&mut self, f: impl FnOnce(&mut crucible_term_core::Session)) {
        let before = self.session.display_offset();
        f(&mut self.session);
        if self.session.display_offset() != before {
            self.full = true;
            self.request_redraw();
        }
    }

    /// Typing (or a paste) returns the viewport to the live screen.
    fn snap_to_bottom(&mut self) {
        if self.session.display_offset() > 0 {
            self.scroll_viewport(|s| s.scroll_to_bottom());
        }
    }

    // ---- pointer ------------------------------------------------------------------------

    /// The grid cell `(col, row)` under the pointer, clamped to the grid.
    fn pointer_cell(&self) -> (usize, usize) {
        let s = self.effective_scale();
        let (cols, rows) = self.session.size();
        render::cell_at(
            self.inp.pos.0 * s,
            self.inp.pos.1 * s,
            &self.metrics(),
            self.pad_px(),
            cols,
            rows,
        )
    }

    fn abs_point(&self, (col, row): (usize, usize)) -> Point {
        Point {
            row: self.session.viewport_top() + row,
            col,
        }
    }

    fn report(&mut self, mods: Mods, action: MouseAction, button: MouseButton) -> bool {
        let modes = self.session.modes();
        let (col, row) = self.pointer_cell();
        let ev = MouseEvent {
            action,
            button,
            mods,
            col,
            row,
        };
        match encode_mouse(ev, &modes) {
            Some(b) => {
                self.send(&b);
                true
            }
            None => false,
        }
    }

    /// The pointer icon this window wants now.
    pub(super) fn desired_icon(&self, shift: bool) -> CursorIcon {
        desired_icon(
            self.session.mouse_icon(),
            self.session.modes().mouse != MouseMode::Off,
            shift,
        )
    }

    fn on_pointer(&mut self, seat: &mut SeatInput, ev: &PointerEvent) {
        self.inp.pos = ev.position;
        let mods = seat.mods;
        let reporting = self.session.modes().mouse != MouseMode::Off && !mods.shift;
        match ev.kind {
            PointerEventKind::Motion { .. } => {
                if self.inp.selecting {
                    self.drag_selection();
                } else if reporting {
                    let held = self.inp.reported.unwrap_or(MouseButton::None);
                    self.report(mods, MouseAction::Motion, held);
                }
            }
            PointerEventKind::Press {
                button,
                serial,
                time,
            } => {
                seat.serial = serial;
                let b = match button {
                    BTN_LEFT => MouseButton::Left,
                    BTN_MIDDLE => MouseButton::Middle,
                    BTN_RIGHT => MouseButton::Right,
                    _ => return,
                };
                if reporting {
                    if self.report(mods, MouseAction::Press, b) {
                        self.inp.reported = Some(b);
                    }
                    return;
                }
                let _ = time;
                match b {
                    MouseButton::Left => self.begin_selection(mods.ctrl),
                    MouseButton::Middle => self.paste_primary(seat),
                    _ => {}
                }
            }
            PointerEventKind::Release { button, serial, .. } => {
                seat.serial = serial;
                let b = match button {
                    BTN_LEFT => MouseButton::Left,
                    BTN_MIDDLE => MouseButton::Middle,
                    BTN_RIGHT => MouseButton::Right,
                    _ => return,
                };
                // A reported press always gets its release, even if Shift is held by now.
                if self.inp.reported == Some(b) {
                    self.inp.reported = None;
                    self.report(mods, MouseAction::Release, b);
                } else if b == MouseButton::Left && self.inp.selecting {
                    self.end_selection(seat, serial);
                }
            }
            PointerEventKind::Axis { vertical, .. } => {
                let units = if vertical.value120 != 0 {
                    f64::from(vertical.value120)
                } else if vertical.discrete != 0 {
                    f64::from(vertical.discrete) * 120.0
                } else {
                    vertical.absolute * 8.0
                };
                self.inp.wheel += units;
                let notches = (self.inp.wheel / 120.0).trunc();
                self.inp.wheel -= notches * 120.0;
                if notches != 0.0 {
                    self.on_wheel(mods, notches as i32);
                }
            }
            PointerEventKind::Enter { serial } => {
                seat.serial = serial;
                seat.set_cursor(self.desired_icon(mods.shift), true);
            }
            PointerEventKind::Leave { .. } => {}
        }
    }

    /// `notches` > 0 is a wheel step down (toward newer output), < 0 up.
    fn on_wheel(&mut self, mods: Mods, notches: i32) {
        let modes = self.session.modes();
        let down = notches > 0;
        let count = notches.unsigned_abs();
        if modes.mouse != MouseMode::Off && !mods.shift {
            let b = if down {
                MouseButton::WheelDown
            } else {
                MouseButton::WheelUp
            };
            for _ in 0..count {
                self.report(mods, MouseAction::Press, b);
            }
        } else if modes.alt_screen && modes.alternate_scroll {
            let key = if down { Key::Down } else { Key::Up };
            if let Some(seq) = encode_key(key, Mods::default(), &modes) {
                for _ in 0..count * WHEEL_LINES as u32 {
                    self.send(&seq);
                }
            }
        } else {
            let lines = WHEEL_LINES * notches.saturating_abs();
            self.scroll_viewport(|s| s.scroll_display(if down { -lines } else { lines }));
        }
    }

    // ---- selection ----------------------------------------------------------------------

    fn begin_selection(&mut self, ctrl: bool) {
        let cell = self.pointer_cell();
        let p = self.abs_point(cell);
        let n = self
            .inp
            .clicks
            .press(std::time::Instant::now(), p, MULTI_CLICK);
        let had = self.inp.selection.take().is_some();
        self.inp.pending = None;
        let kind = match n {
            2 => Some(Kind::Word),
            3 => Some(Kind::Line),
            _ => None,
        };
        let rows = |r: usize| self.session.abs_row(r);
        match kind {
            Some(k) => {
                self.inp.selection = Some(Selection::start(k, p, &rows));
                self.inp.selecting = true;
            }
            None => {
                let k = if ctrl { Kind::Block } else { Kind::Char };
                self.inp.pending = Some((k, p));
                self.inp.selecting = true;
            }
        }
        if had || self.inp.selection.is_some() {
            self.full = true;
            self.request_redraw();
        }
    }

    fn drag_selection(&mut self) {
        let p = self.abs_point(self.pointer_cell());
        let rows = |r: usize| self.session.abs_row(r);
        if let Some((kind, origin)) = self.inp.pending
            && p != origin
        {
            self.inp.selection = Some(Selection::start(kind, origin, &rows));
            self.inp.pending = None;
        }
        if let Some(sel) = &mut self.inp.selection {
            let before = sel.clone();
            sel.update(p, &rows);
            if *sel == before {
                return;
            }
        } else {
            return;
        }
        self.full = true;
        self.request_redraw();
    }

    fn end_selection(&mut self, seat: &mut SeatInput, serial: u32) {
        self.inp.selecting = false;
        self.inp.pending = None;
        let Some(sel) = &self.inp.selection else {
            return;
        };
        let text = sel.text(&|r| self.session.abs_row(r));
        if !text.is_empty() {
            seat.set_primary(text, serial);
        }
    }

    // ---- clipboard and primary selection -----------------------------------------------

    fn copy_selection(&mut self, seat: &mut SeatInput) {
        let Some(sel) = &self.inp.selection else {
            return;
        };
        let text = sel.text(&|r| self.session.abs_row(r));
        if !text.is_empty() {
            let serial = seat.serial;
            seat.set_clipboard(text, serial);
        }
    }

    /// Apply the OSC 52 policy to a core event; `focused`: this window holds keyboard focus.
    pub(super) fn on_clipboard_event(
        &mut self,
        seat: &mut SeatInput,
        ev: &crucible_term_core::Event,
        focused: bool,
    ) {
        let serial = seat.serial;
        match clipboard::osc52_policy(ev, focused) {
            Osc52::Clipboard(t) => seat.set_clipboard(t, serial),
            Osc52::Primary(t) => seat.set_primary(t, serial),
            Osc52::Ignore => {}
        }
    }

    fn paste_clipboard(&mut self, seat: &SeatInput) {
        let Some(offer) = seat
            .data_device
            .as_ref()
            .and_then(|d| d.data().selection_offer())
        else {
            return;
        };
        let Some(mime) = offer.with_mime_types(clipboard::pick_mime) else {
            return;
        };
        match offer.receive(mime.to_string()) {
            Ok(pipe) => self.read_paste(pipe),
            Err(e) => eprintln!("crucible-term: paste: {e}"),
        }
    }

    fn paste_primary(&mut self, seat: &SeatInput) {
        let Some(offer) = seat
            .primary_device
            .as_ref()
            .and_then(|d| d.data().selection_offer())
        else {
            return;
        };
        let Some(mime) = offer.with_mime_types(clipboard::pick_mime) else {
            return;
        };
        match offer.receive(mime.to_string()) {
            Ok(pipe) => self.read_paste(pipe),
            Err(e) => eprintln!("crucible-term: paste: {e}"),
        }
    }

    /// Read a selection pipe to EOF on the event loop (never blocking it), then paste into
    /// this window (if it still exists).
    fn read_paste(&mut self, pipe: ReadPipe) {
        let mut data: Vec<u8> = Vec::new();
        let id = self.id;
        let r = self.handle.insert_source(pipe, move |_, f, host| {
            let mut chunk = [0u8; 64 * 1024];
            match rustix::io::read(&*f, &mut chunk) {
                Ok(0) => {
                    if let Some(t) = host.terms.get_mut(&id) {
                        t.paste(&std::mem::take(&mut data));
                    }
                    PostAction::Remove
                }
                Ok(n) => {
                    data.extend_from_slice(&chunk[..n]);
                    if data.len() > MAX_PASTE {
                        eprintln!("crucible-term: paste larger than {MAX_PASTE} bytes dropped");
                        PostAction::Remove
                    } else {
                        PostAction::Continue
                    }
                }
                Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::INTR => {
                    PostAction::Continue
                }
                Err(e) => {
                    eprintln!("crucible-term: paste read: {e}");
                    PostAction::Remove
                }
            }
        });
        if let Err(e) = r {
            eprintln!("crucible-term: paste source: {e}");
        }
    }

    fn paste(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let bytes = clipboard::paste_bytes(data, self.session.modes().bracketed_paste);
        self.snap_to_bottom();
        self.send(&bytes);
    }
}

/// Answer a data request on a background thread so a slow reader cannot stall the UI.
fn serve(fd: OwnedFd, text: Arc<str>) {
    let _ = std::thread::Builder::new()
        .name("selection-write".into())
        .spawn(move || {
            let _ = File::from(fd).write_all(text.as_bytes());
        });
}

impl Host {
    /// Key repeat from sctk's calloop timer: goes to the window holding keyboard focus.
    pub(super) fn on_key_repeat(&mut self, event: &KeyEvent) {
        self.key_to_focused(event, true);
    }

    fn key_to_focused(&mut self, event: &KeyEvent, repeat: bool) {
        let Some(id) = self.routes.focused() else {
            return;
        };
        if let Some(t) = self.terms.get_mut(&id) {
            t.on_key(&mut self.seat_in, event, repeat);
        }
    }
}

// ---- handlers ---------------------------------------------------------------------------

impl KeyboardHandler for Host {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.seat_in.serial = serial;
        let Some(id) = self.routes.enter(&surface.id()) else {
            return;
        };
        let Some(t) = self.terms.get_mut(&id) else {
            return;
        };
        t.focused = true;
        if t.session.modes().focus_events {
            t.send(b"\x1b[I");
        }
        t.arm_blink();
        t.request_redraw();
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.seat_in.mods = Mods::default();
        let Some(id) = self.routes.leave(&surface.id()) else {
            return;
        };
        let Some(t) = self.terms.get_mut(&id) else {
            return;
        };
        t.focused = false;
        if t.session.modes().focus_events {
            t.send(b"\x1b[O");
        }
        t.request_redraw();
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.seat_in.serial = serial;
        self.key_to_focused(&event, false);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key_to_focused(&event, true);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        m: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.seat_in.mods = Mods {
            shift: m.shift,
            alt: m.alt,
            ctrl: m.ctrl,
            logo: m.logo,
        };
        if let Some(id) = self.seat_in.cursor.term {
            self.refresh_cursor(id);
        }
    }
}

impl Host {
    /// Recompute the pointer icon for window `id` (after a PTY feed or a Shift change) and set
    /// it when the pointer is in that window and the icon differs from the last one set.
    pub(super) fn refresh_cursor(&mut self, id: TermId) {
        if self.seat_in.cursor.term != Some(id) {
            return;
        }
        if let Some(t) = self.terms.get(&id) {
            let icon = t.desired_icon(self.seat_in.mods.shift);
            self.seat_in.set_cursor(icon, false);
        }
    }
}

impl PointerHandler for Host {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            let Some(id) = self.routes.term(&ev.surface.id()) else {
                continue;
            };
            match ev.kind {
                PointerEventKind::Enter { .. } => self.seat_in.cursor.term = Some(id),
                PointerEventKind::Leave { .. } if self.seat_in.cursor.term == Some(id) => {
                    self.seat_in.cursor.leave();
                }
                _ => {}
            }
            if let Some(t) = self.terms.get_mut(&id) {
                t.on_pointer(&mut self.seat_in, ev);
            }
        }
    }
}

impl DataDeviceHandler for Host {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for Host {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for Host {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        if self
            .seat_in
            .copy_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
            && MIME_TYPES.contains(&mime.as_str())
        {
            serve(OwnedFd::from(fd), self.seat_in.clip_text.clone());
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if self
            .seat_in
            .copy_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            self.seat_in.copy_source = None;
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl PrimarySelectionDeviceHandler for Host {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl PrimarySelectionSourceHandler for Host {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime: String,
        fd: WritePipe,
    ) {
        if self
            .seat_in
            .primary_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
            && MIME_TYPES.contains(&mime.as_str())
        {
            serve(OwnedFd::from(fd), self.seat_in.primary_text.clone());
        }
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        if self
            .seat_in
            .primary_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            self.seat_in.primary_source = None;
        }
    }
}

delegate_keyboard!(Host);
delegate_pointer!(Host);
delegate_data_device!(Host);
delegate_primary_selection!(Host);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_icon_rules() {
        // 1. The application's OSC 22 icon wins over everything.
        assert_eq!(
            desired_icon(Some(CursorIcon::Pointer), true, false),
            CursorIcon::Pointer
        );
        assert_eq!(
            desired_icon(Some(CursorIcon::Pointer), false, true),
            CursorIcon::Pointer
        );
        // 2. Mouse reporting on, no Shift: the arrow.
        assert_eq!(desired_icon(None, true, false), CursorIcon::Default);
        // 3. Otherwise the I-beam, and Shift overrides reporting.
        assert_eq!(desired_icon(None, false, false), CursorIcon::Text);
        assert_eq!(desired_icon(None, true, true), CursorIcon::Text);
    }

    #[test]
    fn keysyms_translate_to_core_keys() {
        assert_eq!(
            translate(Keysym::Return, Some("\r")),
            Some((Key::Enter, false))
        );
        assert_eq!(
            translate(Keysym::ISO_Left_Tab, None),
            Some((Key::Tab, true))
        );
        assert_eq!(translate(Keysym::F5, None), Some((Key::F(5), false)));
        assert_eq!(translate(Keysym::Page_Up, None), Some((Key::PageUp, false)));
        assert_eq!(
            translate(Keysym::a, Some("a")),
            Some((Key::Char('a'), false))
        );
        // Ctrl+a: the compositor's text is a control character, so the keysym decides.
        assert_eq!(
            translate(Keysym::a, Some("\u{1}")),
            Some((Key::Char('a'), false))
        );
        // A dead-key composition arrives as text on a different keysym.
        assert_eq!(
            translate(Keysym::e, Some("é")),
            Some((Key::Char('é'), false))
        );
        assert_eq!(translate(Keysym::Shift_L, None), None);
    }
}

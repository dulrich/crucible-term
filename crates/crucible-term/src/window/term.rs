//! One toplevel window of a [`Host`]: its surface, buffers, session, PTY and per-window input
//! state. Calloop sources a `Term` registers carry its [`TermId`] and look it up in the host on
//! every wakeup (gone: the source drops itself).
//!
//! Frame pacing: [`Term::request_redraw`] marks the window dirty; a frame is drawn only when
//! dirty and no frame callback is outstanding ([`Term::draw`]); the `wl_surface.frame` callback
//! draws again only if new damage arrived meanwhile.

use super::host::Host;
use super::lifecycle::{self, BufState, Lifecycle};
use super::routing::TermId;
use super::{Config, damage, input};
use crate::font::{FontPool, Metrics, PooledFonts};
use crate::outbox::Outbox;
use crate::render::{self, Canvas, CursorDraw, Pad, Theme};
use crate::selection::{invalidated, merge_damage};
use crucible_term_core::pty::Pty;
use crucible_term_core::{Damage, Event, Session};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::window::Window;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::slot::{Buffer, Slot, SlotPool};
use std::io::ErrorKind;
use std::time::{Duration, Instant};
use wayland_client::QueueHandle;
use wayland_client::protocol::wl_shm;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

/// Mode 2026: hold presenting at most this long while the child keeps sync set.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Cursor blink half-period.
const BLINK: Duration = Duration::from_millis(500);
/// Poll period for held buffers to be released after a suspend (only while some are held).
const DRAIN_POLL: Duration = Duration::from_millis(50);

/// The surface half of a [`Term`]: everything bound to one Wayland connection. Dropped (the
/// session half stays) when the connection is lost; a rebind builds a new one.
pub struct Surf {
    pub(super) pool: SlotPool,
    /// Live shm buffers, oldest first, each with the number of the frame it holds (`None`:
    /// never painted). The paint target is the buffer itself; there is no frame image.
    pub(super) buffers: Vec<(Buffer, Option<u64>)>,
    /// Committed-frame numbering and the last frame's damage, for buffer-age repair.
    pub(super) frames: damage::FrameLog,
    pub(super) window: Window,
    pub(super) viewport: Option<WpViewport>,
    pub(super) fractional: Option<WpFractionalScaleV1>,
    pub(super) qh: QueueHandle<Host>,
    pub(super) configured: bool,
    pub(super) frame_pending: bool,
    /// Suspended state and the held buffers still draining (see [`lifecycle`]).
    pub(super) life: Lifecycle,
    /// Full-size buffers dropped from `buffers` on suspend that the compositor still held; each
    /// is destroyed once its release is observed, then the pool may be recreated.
    pub(super) draining: Vec<Buffer>,
    /// The drain poll timer is registered.
    pub(super) drain_timer: bool,
    /// The 1×1 placeholder (from the host pool) attached while suspended.
    pub(super) tiny: Option<Buffer>,
}

impl Surf {
    /// A fresh surface half for `window` (role created and committed by the caller).
    pub(super) fn new(
        pool: SlotPool,
        window: Window,
        viewport: Option<WpViewport>,
        fractional: Option<WpFractionalScaleV1>,
        qh: QueueHandle<Host>,
    ) -> Surf {
        Surf {
            pool,
            buffers: Vec::new(),
            frames: damage::FrameLog::default(),
            window,
            viewport,
            fractional,
            qh,
            configured: false,
            frame_pending: false,
            life: Lifecycle::default(),
            draining: Vec::new(),
            drain_timer: false,
            tiny: None,
        }
    }

    /// Destroy the scaling objects; the window, surface and buffers go on drop.
    pub(super) fn teardown(self) {
        if let Some(vp) = self.viewport {
            vp.destroy();
        }
        if let Some(f) = self.fractional {
            f.destroy();
        }
    }
}

/// Per-window state: the session half (`Session`, PTY, pidfd and PTY sources, outbox, exit)
/// plus an optional surface half ([`Surf`], `None` while detached).
pub struct Term {
    pub(super) id: TermId,
    /// The surface half; `None` while detached (no connection, or not rebound yet).
    pub(super) surf: Option<Surf>,
    pub(super) handle: LoopHandle<'static, Host>,
    pub(super) cfg: Config,
    pub(super) session: Session,
    pub(super) pty: Pty,
    /// Every byte for the child queues here (see [`Term::send`]).
    pub(super) outbox: Outbox,
    /// The writable-interest source draining the outbox, while registered.
    pub(super) write_source: Option<RegistrationToken>,
    /// The PTY master and pidfd sources (removed when the window goes).
    pub(super) sources: Vec<RegistrationToken>,
    /// Damage taken from the core after PTY output, held until the next paint.
    pub(super) pending_damage: Option<Damage>,
    pub(super) inp: input::Input,
    pub(super) fonts: PooledFonts,
    pub(super) theme: Theme,
    /// Logical window size.
    pub(super) logical: (u32, u32),
    /// Fractional scale (1.0 without the protocol).
    pub(super) scale: f64,
    /// Integer `wl_output` scale, used only without fractional scale.
    pub(super) int_scale: i32,
    pub(super) has_fractional: bool,
    /// Buffer size in pixels.
    pub(super) buf: (u32, u32),
    pub(super) dirty: bool,
    pub(super) full: bool,
    pub(super) focused: bool,
    pub(super) blink_on: bool,
    pub(super) blink_armed: bool,
    /// When the current mode 2026 hold started.
    pub(super) sync_since: Option<Instant>,
    pub(super) sync_timer: bool,
    pub(super) last_cursor: Option<usize>,
    /// The child's exit code once known; the host then removes the window.
    pub(super) exit: Option<i32>,
    /// The compositor asked to close; the host removes the window and reaps the child.
    pub(super) close_requested: bool,
    /// `CTERM_DEBUG_MEM=1`: print memory attribution to stderr (first frame, then 4 s idle).
    pub(super) mem_debug: bool,
    pub(super) mem_first: bool,
    /// `CTERM_DEBUG_FRAME_SUM=1`: log a checksum of every committed full-size buffer.
    pub(super) sum_debug: bool,
    /// Log a forced-full-repaint checksum after the next commit (set on resume).
    pub(super) sum_full_pending: bool,
    /// Full-size commits so far (the frame-sum sequence number; never reset).
    pub(super) commits: u64,
    /// The next draw presents even inside a mode 2026 hold (the resume repaint).
    pub(super) force_present: bool,
}

pub(super) fn exit_code(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}

/// Copy full-width bands of rows (`(y, height)`, `stride` bytes per row) from one slot of the
/// pool to another, one row at a time through a row-sized scratch (the two slots share one
/// mapping, so they cannot be borrowed together; a frame-sized temporary would be the frame
/// image again). The source may still be held by the compositor: it is only read.
fn copy_bands(
    pool: &mut SlotPool,
    src: &Slot,
    dst: &Slot,
    stride: usize,
    bands: &[(usize, usize)],
) {
    let mut row = vec![0u8; stride];
    for &(y, h) in bands {
        for r in y..y + h {
            let at = r * stride;
            let Some(s) = pool.raw_data_mut(src).get(at..at + stride) else {
                break;
            };
            row.copy_from_slice(s);
            let Some(d) = pool.raw_data_mut(dst).get_mut(at..at + stride) else {
                break;
            };
            d.copy_from_slice(&row);
        }
    }
}

/// View a shm canvas as `Argb8888` pixels. The format is little-endian, so on a little-endian
/// host a native `u32` is the wire layout; big-endian hosts are not supported (no draw).
fn as_pixels(bytes: &mut [u8]) -> Option<&mut [u32]> {
    if cfg!(target_endian = "big") {
        return None;
    }
    // SAFETY: every bit pattern is a valid `u32`; `align_to_mut` only yields the aligned middle.
    #[allow(unsafe_code)]
    let (head, px, tail) = unsafe { bytes.align_to_mut::<u32>() };
    (head.is_empty() && tail.is_empty()).then_some(px)
}

impl Term {
    pub(super) fn effective_scale(&self) -> f64 {
        if self.has_fractional {
            self.scale
        } else {
            f64::from(self.int_scale)
        }
    }

    pub(super) fn metrics(&self) -> Metrics {
        self.fonts.fonts.borrow().metrics()
    }

    /// The configured logical padding in buffer pixels (sizes the grid).
    fn base_pad(&self) -> Pad {
        let s = self.effective_scale();
        let px = |v: u32| (f64::from(v) * s).round() as u32;
        Pad {
            x: px(self.cfg.settings.pad.0),
            y: px(self.cfg.settings.pad.1),
        }
    }

    /// The padding in buffer pixels: the configured logical padding scaled, plus the spare
    /// pixels split evenly when `center` is set.
    pub(super) fn pad_px(&self) -> Pad {
        let base = self.base_pad();
        if self.cfg.settings.center {
            render::centered(
                self.buf.0,
                self.buf.1,
                &self.metrics(),
                self.session.size(),
                base,
            )
        } else {
            base
        }
    }

    /// `kbd_focus`: this window holds keyboard focus (the OSC 52 rule).
    pub(super) fn on_pty_readable(&mut self, seat: &mut input::SeatInput, kbd_focus: bool) {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0;
        let history_before = self.session.history_len();
        loop {
            match self.pty.read(&mut buf) {
                Ok(0) => return self.child_gone(),
                Ok(n) => {
                    self.session.feed(&buf[..n]);
                    total += n;
                    if total >= 4 << 20 {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return self.child_gone(),
            }
        }
        let replies = self.session.take_replies();
        self.send(&replies);
        for ev in self.session.take_events() {
            match ev {
                Event::Title(t) => {
                    if let Some(s) = &self.surf {
                        s.window.set_title(if t.is_empty() {
                            self.cfg.title.clone()
                        } else {
                            t
                        });
                    }
                }
                other => self.on_clipboard_event(seat, &other, kbd_focus),
            }
        }
        // Output over selected rows (or scrolling the history) makes the selection stale.
        let damage = self.session.take_damage();
        if let Some(sel) = &self.inp.selection
            && invalidated(
                sel,
                &damage,
                self.session.history_len(),
                self.session.history_len() != history_before,
            )
        {
            self.inp.selection = None;
            self.full = true;
        }
        self.pending_damage = Some(match self.pending_damage.take() {
            Some(p) => merge_damage(p, damage),
            None => damage,
        });
        self.blink_on = true;
        self.arm_blink();
        self.request_redraw();
    }

    /// The pidfd fired: the child has exited. Drain what the PTY still holds, then close.
    pub(super) fn on_child_exited(&mut self, seat: &mut input::SeatInput, kbd_focus: bool) {
        if self.exit.is_none() {
            self.on_pty_readable(seat, kbd_focus);
        }
        if self.exit.is_none() {
            self.child_gone();
        }
    }

    /// Seed the session's cursor with the configured shape and blink, as if the child had sent
    /// DECSCUSR (which still overrides it later; `CSI 0 q` resets to a steady block).
    pub(super) fn apply_cursor_defaults(&mut self) {
        use crucible_term_core::CursorKind;
        let s = &self.cfg.settings;
        let base = match s.cursor_shape {
            CursorKind::Underline => 3,
            CursorKind::Beam => 5,
            _ => 1,
        };
        let n = if s.cursor_blink { base } else { base + 1 };
        self.session.feed(format!("\x1b[{n} q").as_bytes());
        self.arm_blink();
    }

    fn child_gone(&mut self) {
        let code = self.pty.wait().map(exit_code).unwrap_or(1);
        self.exit = Some(code);
    }

    pub(super) fn arm_blink(&mut self) {
        if self.blink_armed || !self.session.cursor_style().blinking {
            return;
        }
        self.blink_armed = true;
        let id = self.id;
        let r = self
            .handle
            .insert_source(Timer::from_duration(BLINK), move |_, _, host| {
                let Some(t) = host.terms.get_mut(&id) else {
                    return TimeoutAction::Drop;
                };
                if !t.session.cursor_style().blinking || !t.focused {
                    t.blink_on = true;
                    t.blink_armed = false;
                    t.mark_cursor();
                    return TimeoutAction::Drop;
                }
                t.blink_on = !t.blink_on;
                t.mark_cursor();
                TimeoutAction::ToDuration(BLINK)
            });
        if r.is_err() {
            self.blink_armed = false;
        }
    }

    pub(super) fn mark_cursor(&mut self) {
        self.request_redraw();
    }

    pub(super) fn request_redraw(&mut self) {
        self.dirty = true;
        if !self.surf.as_ref().is_some_and(|s| s.frame_pending) {
            self.draw();
        }
    }

    /// Recompute buffer and grid sizes after a configure or scale change. A scale change moves
    /// the window to the font pool entry for that scale.
    pub(super) fn relayout(&mut self, fonts: &mut FontPool) {
        let s = self.effective_scale();
        if let Err(e) = fonts.rescale(&mut self.fonts, s as f32) {
            eprintln!("crucible-term: font: {e}; keeping the previous scale");
        }
        let buf = (
            ((f64::from(self.logical.0) * s).round() as u32).max(1),
            ((f64::from(self.logical.1) * s).round() as u32).max(1),
        );
        if buf != self.buf {
            self.buf = buf;
            if let Some(sf) = self.surf.as_mut() {
                sf.buffers.clear();
                sf.frames.reset();
            }
        }
        let m: Metrics = self.metrics();
        let (cols, rows) = render::grid_size(buf.0, buf.1, &m, self.base_pad());
        if (cols, rows) != self.session.size() {
            self.inp.selection = None;
            self.session.resize(cols, rows);
            let size = (
                u16::try_from(cols).unwrap_or(u16::MAX),
                u16::try_from(rows).unwrap_or(u16::MAX),
            );
            if let Err(e) = self.pty.resize(size) {
                eprintln!("crucible-term: pty resize: {e}");
            }
        }
        if let Some(s) = &self.surf {
            if let Some(vp) = &s.viewport {
                vp.set_destination(self.logical.0 as i32, self.logical.1 as i32);
            } else {
                s.window.wl_surface().set_buffer_scale(self.int_scale);
            }
        }
        self.full = true;
        self.request_redraw();
    }

    /// Paint and present one frame if allowed: configured, dirty, no frame callback pending,
    /// and not inside a mode 2026 hold younger than [`SYNC_TIMEOUT`].
    pub(super) fn draw(&mut self) {
        // Detached (no surface): the session keeps parsing PTY output; nothing is drawn.
        if let Some(mut s) = self.surf.take() {
            self.draw_on(&mut s);
            self.surf = Some(s);
        }
    }

    fn draw_on(&mut self, s: &mut Surf) {
        // Suspended: nothing is drawn; PTY output keeps feeding the session and marking dirty.
        if !s.configured || !self.dirty || s.frame_pending || !s.life.may_draw() {
            return;
        }
        let force = std::mem::take(&mut self.force_present);
        if self.session.synchronized_output() && !force {
            let since = *self.sync_since.get_or_insert_with(Instant::now);
            let left = SYNC_TIMEOUT.saturating_sub(since.elapsed());
            if !left.is_zero() {
                if !self.sync_timer {
                    self.sync_timer = true;
                    let id = self.id;
                    let _ =
                        self.handle
                            .insert_source(Timer::from_duration(left), move |_, _, host| {
                                if let Some(t) = host.terms.get_mut(&id) {
                                    t.sync_timer = false;
                                    t.draw();
                                }
                                TimeoutAction::Drop
                            });
                }
                return;
            }
        } else {
            self.sync_since = None;
        }
        self.dirty = false;
        let damage = merge_damage(
            self.pending_damage
                .take()
                .unwrap_or(Damage::Rows(Vec::new())),
            self.session.take_damage(),
        );
        // Scrolled back: damage rows are screen rows, not viewport rows, so repaint everything.
        let scrolled = self.session.display_offset() > 0;
        let grid = self.session.viewport();
        let mut rows = if self.full || scrolled {
            None
        } else {
            render::rows_to_paint(&damage, self.last_cursor, grid.cursor.1)
        };
        self.full = false;
        self.last_cursor = Some(grid.cursor.1);
        if rows.as_ref().is_some_and(Vec::is_empty) {
            return;
        }
        // Paint target: a free shm buffer, first brought up to the last committed frame.
        let Some((buffer, held)) = self.free_buffer(s) else {
            eprintln!("crucible-term: no shm buffer available");
            self.dirty = true;
            self.full = true;
            return;
        };
        let (w, h) = (self.buf.0 as usize, self.buf.1 as usize);
        let current = s.frames.current();
        let source = s
            .buffers
            .iter()
            .find(|(_, f)| *f == Some(current))
            .map(|(b, _)| b.slot());
        match s.frames.repair(held, source.is_some()) {
            damage::Repair::Nothing => {}
            damage::Repair::Repaint => rows = None,
            damage::Repair::Bands(bands) => {
                if let Some(src) = &source {
                    copy_bands(&mut s.pool, src, &buffer.slot(), w * 4, &bands);
                }
            }
            damage::Repair::All => {
                if let Some(src) = &source {
                    copy_bands(&mut s.pool, src, &buffer.slot(), w * 4, &[(0, h)]);
                }
            }
        }
        // Fallback fonts for new text: the only I/O around a frame, kept outside `paint`.
        {
            let mut fonts = self.fonts.fonts.borrow_mut();
            for r in rows.clone().unwrap_or_else(|| (0..grid.rows).collect()) {
                if let Some(row) = grid.screen.get(r) {
                    for c in &row.cells {
                        fonts.prepare(&c.text);
                    }
                }
            }
        }
        let mut style = self.session.cursor_style();
        if scrolled {
            style.shape = crucible_term_core::CursorKind::Hidden;
        }
        let cursor = CursorDraw {
            style,
            focused: self.focused,
            blink_on: self.blink_on,
        };
        let pad = self.pad_px();
        let top = self.session.viewport_top();
        let sel = self.inp.selection.as_ref();
        let selected = |r: usize, c: usize| sel.is_some_and(|s| s.contains(top + r, c));
        let Some(px) = buffer.canvas(&mut s.pool).and_then(as_pixels) else {
            eprintln!("crucible-term: shm buffer not drawable");
            s.buffers.push((buffer, None));
            self.dirty = true;
            self.full = true;
            return;
        };
        let bands = render::paint(
            &mut Canvas {
                px,
                width: w,
                height: h,
            },
            &grid,
            &self.theme,
            &mut self.fonts.fonts.borrow_mut(),
            pad,
            cursor,
            rows.as_deref(),
            sel.is_some().then_some(&selected as render::Selected<'_>),
        );
        if bands.is_empty() {
            // Nothing painted, but the repair above brought the buffer up to the current frame.
            s.buffers.push((buffer, Some(current).filter(|&c| c > 0)));
            return;
        }
        // Debug-only checksums, taken before the attach (the canvas is writable until then).
        if self.sum_debug {
            self.commits += 1;
            if let Some(bytes) = buffer.canvas(&mut s.pool) {
                eprintln!(
                    "crucible-term: frame-sum: {} {} {:016x}",
                    self.id.0,
                    self.commits,
                    lifecycle::fnv1a64(bytes)
                );
            }
            if std::mem::take(&mut self.sum_full_pending) {
                let mut scratch = vec![0u32; w * h];
                render::paint(
                    &mut Canvas {
                        px: &mut scratch,
                        width: w,
                        height: h,
                    },
                    &grid,
                    &self.theme,
                    &mut self.fonts.fonts.borrow_mut(),
                    pad,
                    cursor,
                    None,
                    sel.is_some().then_some(&selected as render::Selected<'_>),
                );
                eprintln!(
                    "crucible-term: frame-sum-full: {} {:016x}",
                    self.id.0,
                    lifecycle::fnv1a64_px(&scratch)
                );
            }
        }
        let surface = s.window.wl_surface().clone();
        for &(y, bh) in &bands {
            surface.damage_buffer(0, y as i32, w as i32, bh as i32);
        }
        surface.frame(&s.qh, surface.clone());
        if let Err(e) = buffer.attach_to(&surface) {
            eprintln!("crucible-term: attach: {e}");
        }
        s.window.commit();
        s.frame_pending = true;
        let n = s.frames.commit(bands);
        s.buffers.push((buffer, Some(n)));
        if s.buffers.len() > 2 {
            s.buffers.remove(0);
        }
        if self.mem_debug && self.mem_first {
            self.mem_first = false;
            self.print_mem(Some(&*s), "first-frame");
        }
    }

    /// The compositor hid the window (configure with `suspended`): show a 1×1 placeholder from
    /// the host pool `tiny`, give up every full-size buffer, and return the pool's memory once
    /// no full-size buffer is held. Rendering stops until [`Term::resume`].
    pub(super) fn suspend(&mut self, tiny: &mut SlotPool, shm: &Shm) {
        if let Some(mut s) = self.surf.take() {
            self.suspend_on(&mut s, tiny, shm);
            self.surf = Some(s);
        }
    }

    fn suspend_on(&mut self, s: &mut Surf, tiny: &mut SlotPool, shm: &Shm) {
        let states: Vec<BufState> = s
            .buffers
            .iter()
            .map(|(b, _)| {
                if s.pool.canvas(b).is_some() {
                    BufState::Released
                } else {
                    BufState::Held
                }
            })
            .collect();
        let plan = s.life.suspend(&states);
        // Placeholder first, so the commit replacing the full-size buffer is on the wire before
        // anything is dropped (the compositor releases the old buffer on that replacement).
        match tiny.create_buffer(1, 1, 4, wl_shm::Format::Argb8888) {
            Ok((b, canvas)) => {
                canvas.fill(0);
                let surface = s.window.wl_surface().clone();
                if let Some(vp) = &s.viewport {
                    // Keep the window geometry: the 1×1 is scaled to the logical size.
                    vp.set_destination(self.logical.0 as i32, self.logical.1 as i32);
                } else {
                    // A 1×1 buffer at scale > 1 is a protocol error; restored on resume.
                    surface.set_buffer_scale(1);
                }
                if let Err(e) = b.attach_to(&surface) {
                    eprintln!("crucible-term: attach placeholder: {e}");
                }
                surface.damage_buffer(0, 0, 1, 1);
                s.window.commit();
                // The previous placeholder (if any) is destroyed by sctk on its release.
                s.tiny = Some(b);
            }
            Err(e) => eprintln!("crucible-term: placeholder buffer: {e}"),
        }
        // Released buffers are destroyed now (sctk destroys an inactive `Buffer` on drop); held
        // ones wait in `draining` until their release is observed.
        let all: Vec<(Buffer, Option<u64>)> = std::mem::take(&mut s.buffers);
        for (i, (b, _)) in all.into_iter().enumerate() {
            if plan.drain.contains(&i) {
                s.draining.push(b);
            }
        }
        s.frames.reset();
        if plan.recreate_pool {
            Term::recreate_pool(s, shm);
        }
        if self.mem_debug && s.life.suspended() {
            self.print_mem(Some(&*s), "suspended");
        }
        self.arm_drain(s);
    }

    /// The compositor shows the window again: forget the stale frame callback (one requested
    /// before the hide may never fire), restore the integer scale, and force a full repaint into
    /// fresh buffers. The caller's relayout then draws and commits it in the same handler.
    pub(super) fn resume(&mut self) {
        if let Some(mut s) = self.surf.take() {
            self.resume_on(&mut s);
            self.surf = Some(s);
        }
    }

    fn resume_on(&mut self, s: &mut Surf) {
        let Some(plan) = s.life.resume() else {
            return;
        };
        if plan.clear_frame_pending {
            s.frame_pending = false;
        }
        if s.viewport.is_none() {
            s.window.wl_surface().set_buffer_scale(self.int_scale);
        }
        // The compositor releases the placeholder when the full-size buffer replaces it; sctk
        // defers its destroy until then and its slot returns to the host pool.
        s.tiny = None;
        if plan.full_repaint {
            s.frames.reset();
            self.full = true;
            self.dirty = true;
            self.force_present = true;
            self.sum_full_pending = self.sum_debug;
        }
    }

    /// Replace the pool by an empty one, returning its mapping to the OS. Only called when no
    /// buffer of this pool is held or kept (`buffers` and `draining` are empty).
    fn recreate_pool(s: &mut Surf, shm: &Shm) {
        debug_assert!(s.buffers.is_empty() && s.draining.is_empty());
        match SlotPool::new(4096, shm) {
            Ok(p) => s.pool = p,
            Err(e) => eprintln!("crucible-term: recreate pool: {e}"),
        }
    }

    /// Destroy the draining buffers the compositor has released; on the last release while
    /// still suspended, recreate the pool. Returns whether any are left to wait for.
    pub(super) fn poll_drain(&mut self, shm: &Shm) -> bool {
        let Some(mut s) = self.surf.take() else {
            return false;
        };
        let left = self.poll_drain_on(&mut s, shm);
        self.surf = Some(s);
        left
    }

    fn poll_drain_on(&mut self, s: &mut Surf, shm: &Shm) -> bool {
        let mut i = 0;
        let mut recreate = false;
        while i < s.draining.len() {
            if s.pool.canvas(&s.draining[i]).is_some() {
                // Released: dropping an inactive `Buffer` destroys its wl_buffer now.
                drop(s.draining.remove(i));
                recreate |= s.life.release();
            } else {
                i += 1;
            }
        }
        if recreate && s.buffers.is_empty() {
            Term::recreate_pool(s, shm);
            if self.mem_debug {
                self.print_mem(Some(&*s), "suspended-drained");
            }
        }
        debug_assert_eq!(s.draining.len(), s.life.draining());
        s.life.draining() > 0
    }

    /// Poll the draining buffers until all are released (sctk handles `wl_buffer.release`
    /// internally, so the window observes it through the slot state).
    fn arm_drain(&mut self, s: &mut Surf) {
        if s.drain_timer || s.draining.is_empty() {
            return;
        }
        s.drain_timer = true;
        let id = self.id;
        let r = self
            .handle
            .insert_source(Timer::from_duration(DRAIN_POLL), move |_, _, host| {
                let Some(shm) = host.wl.as_ref().map(|w| &w.shm) else {
                    return TimeoutAction::Drop;
                };
                let Some(t) = host.terms.get_mut(&id) else {
                    return TimeoutAction::Drop;
                };
                if t.poll_drain(shm) {
                    TimeoutAction::ToDuration(DRAIN_POLL)
                } else {
                    if let Some(s) = t.surf.as_mut() {
                        s.drain_timer = false;
                    }
                    TimeoutAction::Drop
                }
            });
        if r.is_err() {
            s.drain_timer = false;
        }
    }

    /// Debug-only (`CTERM_DEBUG_MEM=1`): byte sizes of the big per-window and shareable pieces.
    pub(super) fn print_mem(&self, s: Option<&Surf>, when: &str) {
        let (faces, emoji, nfaces, nshapes, nglyphs, gbytes) =
            self.fonts.fonts.borrow().mem_stats();
        let (cols, rows) = self.session.size();
        let buffer_bytes: usize = s
            .iter()
            .flat_map(|s| s.buffers.iter())
            .map(|(b, _)| (b.height() * b.stride()) as usize)
            .sum();
        eprintln!(
            "crucible-term: mem: {when} buffer_bytes={buffer_bytes} buf={}x{} pool_bytes={} live_buffers={}",
            self.buf.0,
            self.buf.1,
            s.map_or(0, |s| s.pool.len()),
            s.map_or(0, |s| s.buffers.len())
        );
        eprintln!(
            "crucible-term: mem: {when} face_bytes={faces} emoji_face_bytes={emoji} faces={nfaces} \
             shape_entries={nshapes} glyph_entries={nglyphs} glyph_bitmap_bytes={gbytes}"
        );
        eprintln!(
            "crucible-term: mem: {when} session_grid_bytes_est={} cols={cols} rows={rows} history_rows={}",
            self.session.grid_bytes_estimate(),
            self.session.history_len()
        );
    }

    /// A buffer the compositor has released, or a new one (double buffering: at most two live;
    /// a third only while the compositor holds both). Returns it with the frame it holds.
    fn free_buffer(&mut self, s: &mut Surf) -> Option<(Buffer, Option<u64>)> {
        if let Some(i) = s
            .buffers
            .iter()
            .position(|(b, _)| s.pool.canvas(b).is_some())
        {
            return Some(s.buffers.remove(i));
        }
        let (w, h) = (self.buf.0 as i32, self.buf.1 as i32);
        s.pool
            .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
            .map(|(b, _)| (b, None))
            .map_err(|e| eprintln!("crucible-term: create buffer: {e}"))
            .ok()
    }

    /// Remove the window: unregister its sources, destroy its scaling objects, drop its surface
    /// and buffers. Returns the PTY so the host can reap the child.
    pub(super) fn teardown(self) -> Pty {
        for t in self.sources {
            self.handle.remove(t);
        }
        if let Some(t) = self.write_source {
            self.handle.remove(t);
        }
        if let Some(s) = self.surf {
            s.teardown();
        }
        self.pty
    }
}

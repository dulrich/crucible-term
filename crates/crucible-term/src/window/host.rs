//! The per-process host: one Wayland connection, registry, globals, seat (keyboard, pointer, key
//! repeat, data device and primary selection), `Shm`, the calloop loop and the font pool, plus
//! every [`Term`] keyed by [`TermId`]. The host is the calloop data; every sctk handler routes to
//! the owning `Term` by `wl_surface` through [`Routes`]. Standalone [`run`] is a host with one
//! window.

use super::input::{self, SeatInput};
use super::routing::{Routes, TermId};
use super::term::{Term, exit_code};
use super::{Config, WindowError, damage, wl};
use crate::font::FontPool;
use crate::outbox::Outbox;
use crucible_term_core::Session;
use crucible_term_core::pty::{Pty, SpawnSpec};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction,
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::csd_frame::WindowState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_shm,
    delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

/// How long a closed window's child gets to exit on SIGHUP before it counts as killed.
const CLOSE_GRACE: Duration = Duration::from_millis(500);
/// Poll period of the close-grace timer (it exists only while a close is pending).
const CLOSE_POLL: Duration = Duration::from_millis(10);

/// The process's windows and everything they share; the calloop event loop's data.
pub struct Host {
    pub(super) registry: RegistryState,
    pub(super) seat: SeatState,
    pub(super) output: OutputState,
    pub(super) shm: Shm,
    pub(super) compositor: CompositorState,
    pub(super) xdg: XdgShell,
    pub(super) viewporter: Option<WpViewporter>,
    pub(super) fractional_mgr: Option<WpFractionalScaleManagerV1>,
    pub(super) qh: QueueHandle<Host>,
    pub(super) handle: LoopHandle<'static, Host>,
    /// Seat-wide input: devices, modifiers, serial, clipboard sources.
    pub(super) seat_in: SeatInput,
    pub(super) fonts: FontPool,
    pub(super) terms: BTreeMap<TermId, Term>,
    pub(super) routes: Routes<ObjectId>,
    next_id: u64,
    /// Exit codes of windows that are gone.
    exits: BTreeMap<TermId, i32>,
    /// Closed windows whose child is still in its SIGHUP grace.
    closing: usize,
}

/// One step of the close grace: the child's `try_wait` result at `now` against `deadline`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Grace {
    /// Not exited yet; poll again.
    Wait,
    /// The exit code to record (`128 + 1` once the deadline passed, as if killed by SIGHUP).
    Done(i32),
}

pub(super) fn grace_step(
    status: std::io::Result<Option<std::process::ExitStatus>>,
    now: Instant,
    deadline: Instant,
) -> Grace {
    match status {
        Ok(Some(s)) => Grace::Done(exit_code(s)),
        Ok(None) if now < deadline => Grace::Wait,
        _ => Grace::Done(128 + 1),
    }
}

fn lp(e: impl std::fmt::Display) -> WindowError {
    WindowError::Loop(e.to_string())
}

/// Spawn `argv` on a PTY (child environment `env`) in a new window and run until the child exits
/// or the window closes. Returns the child's exit code (128 + signal when killed).
pub fn run(
    cfg: &Config,
    argv: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
) -> Result<i32, WindowError> {
    let (mut event_loop, mut host) = Host::new()?;
    let (id, _) = host.open_window(cfg, argv, env)?;
    while !host.terms.is_empty() || host.closing > 0 {
        event_loop.dispatch(None, &mut host).map_err(lp)?;
        host.reap();
    }
    Ok(host.exits.get(&id).copied().unwrap_or(0))
}

impl Host {
    /// Connect to Wayland and bind the globals. The host stays resident with zero windows; the
    /// caller drives the returned loop (the daemon inserts its own sources through
    /// [`EventLoop::handle`]) and calls [`Host::reap`] after each dispatch.
    pub(crate) fn new() -> Result<(EventLoop<'static, Host>, Host), WindowError> {
        let conn = Connection::connect_to_env().map_err(wl)?;
        let (globals, queue) = registry_queue_init::<Host>(&conn).map_err(wl)?;
        let qh = queue.handle();
        let event_loop: EventLoop<Host> = EventLoop::try_new().map_err(lp)?;
        let handle = event_loop.handle();
        WaylandSource::new(conn.clone(), queue)
            .insert(handle.clone())
            .map_err(lp)?;
        let compositor = CompositorState::bind(&globals, &qh).map_err(wl)?;
        let xdg = XdgShell::bind(&globals, &qh).map_err(wl)?;
        let shm = Shm::bind(&globals, &qh).map_err(wl)?;
        let viewporter = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok();
        let fractional_mgr = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
            .ok();
        let host = Host {
            registry: RegistryState::new(&globals),
            seat: SeatState::new(&globals, &qh),
            output: OutputState::new(&globals, &qh),
            shm,
            compositor,
            xdg,
            viewporter,
            fractional_mgr,
            seat_in: SeatInput::new(&globals, &qh),
            qh,
            handle,
            fonts: FontPool::default(),
            terms: BTreeMap::new(),
            routes: Routes::default(),
            next_id: 1,
            exits: BTreeMap::new(),
            closing: 0,
        };
        Ok((event_loop, host))
    }

    /// Open one toplevel running `argv` with environment `env` on a new PTY; returns the window
    /// id and the child (root) pid. Standalone opens exactly one.
    pub(crate) fn open_window(
        &mut self,
        cfg: &Config,
        argv: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
    ) -> Result<(TermId, u32), WindowError> {
        let id = TermId(self.next_id);
        self.next_id += 1;
        let qh = &self.qh;
        let surface = self.compositor.create_surface(qh);
        let (viewport, fractional) = match (&self.viewporter, &self.fractional_mgr) {
            (Some(vp), Some(fm)) => (
                Some(vp.get_viewport(&surface, qh, ())),
                Some(fm.get_fractional_scale(&surface, qh, id)),
            ),
            _ => (None, None),
        };
        let surface_id = surface.id();
        let window = self
            .xdg
            .create_window(surface, WindowDecorations::RequestServer, qh);
        window.set_title(cfg.title.clone());
        window.set_app_id(cfg.app_id.clone());
        window.commit();

        let fonts = self
            .fonts
            .get(&cfg.settings.family, cfg.settings.size_pt, 1.0)
            .map_err(WindowError::Font)?;
        let m = fonts.fonts.borrow().metrics();
        let logical = (
            u32::from(cfg.cols) * m.cell_w + 2 * cfg.settings.pad.0,
            u32::from(cfg.rows) * m.cell_h + 2 * cfg.settings.pad.1,
        );
        let pty = Pty::spawn(&SpawnSpec {
            argv,
            cwd: cfg.cwd.clone(),
            env,
            size: (cfg.cols, cfg.rows),
        })
        .map_err(WindowError::Pty)?;
        let master = pty.master().try_clone().map_err(lp)?;
        // Non-blocking so one wakeup drains everything the child wrote.
        rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK).map_err(lp)?;
        let mut sources = Vec::new();
        sources.push(
            self.handle
                .insert_source(
                    Generic::new(master, Interest::READ, Mode::Level),
                    move |_, _, host| {
                        let focus = host.routes.holds_focus(id);
                        let Some(t) = host.terms.get_mut(&id) else {
                            return Ok(PostAction::Remove);
                        };
                        t.on_pty_readable(&mut host.seat_in, focus);
                        Ok(if t.exit.is_some() {
                            PostAction::Remove
                        } else {
                            PostAction::Continue
                        })
                    },
                )
                .map_err(lp)?,
        );

        // The child's exit also arrives on a pidfd, so a background grandchild holding the
        // slave cannot keep the window open after the shell is gone. Without pidfd the PTY EOF
        // path above still ends the window.
        match rustix::process::Pid::from_raw(pty.pid() as i32)
            .map(|p| rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()))
        {
            Some(Ok(fd)) => {
                sources.push(
                    self.handle
                        .insert_source(
                            Generic::new(fd, Interest::READ, Mode::Level),
                            move |_, _, host| {
                                let focus = host.routes.holds_focus(id);
                                if let Some(t) = host.terms.get_mut(&id) {
                                    t.on_child_exited(&mut host.seat_in, focus);
                                }
                                Ok(PostAction::Remove)
                            },
                        )
                        .map_err(lp)?,
                );
            }
            Some(Err(e)) => eprintln!("crucible-term: pidfd_open: {e}; relying on PTY EOF"),
            None => {}
        }

        let pool = SlotPool::new((logical.0 * logical.1 * 4) as usize, &self.shm).map_err(wl)?;
        let mut term = Term {
            id,
            pool,
            buffers: Vec::new(),
            frames: damage::FrameLog::default(),
            window,
            viewport,
            has_fractional: fractional.is_some(),
            fractional,
            qh: self.qh.clone(),
            handle: self.handle.clone(),
            cfg: cfg.clone(),
            session: Session::new(
                usize::from(cfg.cols),
                usize::from(cfg.rows),
                cfg.settings.scrollback,
            ),
            pty,
            outbox: Outbox::default(),
            write_source: None,
            sources,
            pending_damage: None,
            inp: input::Input::default(),
            fonts,
            theme: cfg.settings.theme.clone(),
            logical,
            scale: 1.0,
            int_scale: 1,
            buf: (0, 0),
            configured: false,
            dirty: false,
            full: true,
            frame_pending: false,
            focused: true,
            blink_on: true,
            blink_armed: false,
            sync_since: None,
            sync_timer: false,
            last_cursor: None,
            exit: None,
            close_requested: false,
            mem_debug: std::env::var_os("CTERM_DEBUG_MEM").is_some_and(|v| v == "1"),
            mem_first: true,
        };
        term.apply_cursor_defaults();
        if term.mem_debug {
            let _ = self.handle.insert_source(
                Timer::from_duration(Duration::from_secs(4)),
                move |_, _, host| {
                    if let Some(t) = host.terms.get(&id) {
                        t.print_mem("idle-4s");
                    }
                    TimeoutAction::Drop
                },
            );
        }
        let root_pid = term.pty.pid();
        self.routes.insert(surface_id, id);
        self.terms.insert(id, term);
        Ok((id, root_pid))
    }

    /// Ask window `id` to close, as the compositor's close request does: the next [`Host::reap`]
    /// removes it and SIGHUPs its child with the close grace.
    pub(crate) fn close(&mut self, id: TermId) {
        if let Some(t) = self.terms.get_mut(&id) {
            t.close_requested = true;
        }
    }

    /// Take the exit codes recorded since the last call.
    pub(crate) fn drain_exits(&mut self) -> Vec<(TermId, i32)> {
        std::mem::take(&mut self.exits).into_iter().collect()
    }

    /// No window open and no child in its close grace.
    pub(crate) fn is_idle(&self) -> bool {
        self.terms.is_empty() && self.closing == 0
    }

    /// Remove every window whose child exited or that was asked to close. A closed window's
    /// surface goes at once; its child is reaped asynchronously ([`Host::begin_grace`]).
    pub(crate) fn reap(&mut self) {
        let done: Vec<TermId> = self
            .terms
            .iter()
            .filter(|(_, t)| t.exit.is_some() || t.close_requested)
            .map(|(&id, _)| id)
            .collect();
        for id in done {
            let Some(t) = self.terms.remove(&id) else {
                continue;
            };
            self.routes.remove_term(id);
            let exit = t.exit;
            let pty = t.teardown();
            match exit {
                Some(code) => {
                    self.exits.insert(id, code);
                }
                None => self.begin_grace(id, pty),
            }
        }
        self.fonts.prune();
    }

    /// SIGHUP the child, then poll `try_wait` on a host timer until it exits or the grace
    /// deadline passes. Never blocks the loop the other windows share.
    fn begin_grace(&mut self, id: TermId, mut pty: Pty) {
        if let Some(pid) = rustix::process::Pid::from_raw(pty.pid() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::HUP);
        }
        let deadline = Instant::now() + CLOSE_GRACE;
        self.closing += 1;
        let r =
            self.handle
                .insert_source(Timer::from_duration(Duration::ZERO), move |_, _, host| {
                    let status = pty.try_wait();
                    let exited = matches!(status, Ok(Some(_)));
                    match grace_step(status, Instant::now(), deadline) {
                        Grace::Wait => TimeoutAction::ToDuration(CLOSE_POLL),
                        Grace::Done(code) => {
                            if !exited {
                                // Past the deadline: SIGKILL and reap at once, so a long-lived
                                // daemon keeps no zombie. The recorded code stays 128 + 1.
                                if let Some(pid) = rustix::process::Pid::from_raw(pty.pid() as i32)
                                {
                                    let _ = rustix::process::kill_process(
                                        pid,
                                        rustix::process::Signal::KILL,
                                    );
                                }
                                let _ = pty.wait();
                            }
                            host.exits.insert(id, code);
                            host.closing -= 1;
                            TimeoutAction::Drop
                        }
                    }
                });
        if r.is_err() {
            self.closing -= 1;
            self.exits.insert(id, 128 + 1);
        }
    }

    /// The window owning `surface`.
    pub(super) fn term_of(&mut self, surface: &wl_surface::WlSurface) -> Option<&mut Term> {
        let id = self.routes.term(&surface.id())?;
        self.terms.get_mut(&id)
    }
}

impl CompositorHandler for Host {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        let Some(id) = self.routes.term(&surface.id()) else {
            return;
        };
        let Some(t) = self.terms.get_mut(&id) else {
            return;
        };
        if !t.has_fractional && factor != t.int_scale {
            t.int_scale = factor.max(1);
            t.relayout(&mut self.fonts);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(t) = self.term_of(surface) {
            t.frame_pending = false;
            t.draw();
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for Host {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if let Some(t) = self.term_of(window.wl_surface()) {
            t.close_requested = true;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let Some(id) = self.routes.term(&window.wl_surface().id()) else {
            return;
        };
        let Some(t) = self.terms.get_mut(&id) else {
            return;
        };
        if let (Some(w), Some(h)) = configure.new_size {
            t.logical = (w.get(), h.get());
        }
        let focused = configure.state.contains(WindowState::ACTIVATED);
        if focused != t.focused {
            t.focused = focused;
            t.arm_blink();
        }
        t.configured = true;
        t.relayout(&mut self.fonts);
    }
}

impl OutputHandler for Host {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for Host {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seat_in.bind_devices(qh, &seat);
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        match cap {
            Capability::Keyboard if self.seat_in.keyboard.is_none() => {
                // xkbcommon keymap from the compositor; sctk's calloop timer drives repeat
                // from `repeat_info` (no busy loop, no timer while no key is held). Repeat
                // goes to the window holding keyboard focus.
                let kbd = self.seat.get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.handle.clone(),
                    Box::new(|host: &mut Host, _, event| host.on_key_repeat(&event)),
                );
                match kbd {
                    Ok(k) => self.seat_in.keyboard = Some(k),
                    Err(e) => eprintln!("crucible-term: keyboard: {e}"),
                }
            }
            Capability::Pointer if self.seat_in.pointer.is_none() => {
                match self.seat.get_pointer(qh, &seat) {
                    Ok(p) => self.seat_in.pointer = Some(p),
                    Err(e) => eprintln!("crucible-term: pointer: {e}"),
                }
            }
            _ => {}
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        cap: Capability,
    ) {
        match cap {
            Capability::Keyboard => {
                if let Some(k) = self.seat_in.keyboard.take()
                    && k.version() >= 3
                {
                    k.release();
                }
            }
            Capability::Pointer => {
                if let Some(p) = self.seat_in.pointer.take()
                    && p.version() >= 3
                {
                    p.release();
                }
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for Host {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Host {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<WpViewporter, ()> for Host {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for Host {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for Host {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The per-surface fractional scale object carries its window's id.
impl Dispatch<WpFractionalScaleV1, TermId> for Host {
    fn event(
        host: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &TermId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let Some(t) = host.terms.get_mut(id) else {
                return;
            };
            let s = f64::from(scale) / 120.0;
            if (s - t.scale).abs() > f64::EPSILON {
                t.scale = s;
                t.relayout(&mut host.fonts);
            }
        }
    }
}

delegate_compositor!(Host);
delegate_output!(Host);
delegate_shm!(Host);
delegate_seat!(Host);
delegate_xdg_shell!(Host);
delegate_xdg_window!(Host);
delegate_registry!(Host);

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// The close grace as a pure step: an exit inside the window is the child's code, an
    /// unexited child past the deadline (or a wait error) counts as killed by SIGHUP.
    #[test]
    fn close_grace_steps_to_exit_or_sighup_code() {
        let now = Instant::now();
        let later = now + CLOSE_GRACE;
        let exited = std::process::ExitStatus::from_raw(3 << 8);
        assert_eq!(grace_step(Ok(Some(exited)), now, later), Grace::Done(3));
        let killed = std::process::ExitStatus::from_raw(15);
        assert_eq!(grace_step(Ok(Some(killed)), now, later), Grace::Done(143));
        assert_eq!(grace_step(Ok(None), now, later), Grace::Wait);
        assert_eq!(grace_step(Ok(None), later, later), Grace::Done(129));
        let err = std::io::Error::other("x");
        assert_eq!(grace_step(Err(err), now, later), Grace::Done(129));
    }
}

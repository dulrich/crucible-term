//! The per-process host: one Wayland connection, registry, globals, seat (keyboard, pointer, key
//! repeat, data device and primary selection), `Shm`, the calloop loop and the font pool, plus
//! every [`Term`] keyed by [`TermId`]. The host is the calloop data; every sctk handler routes to
//! the owning `Term` by `wl_surface` through [`Routes`]. Standalone [`run`] is a host with one
//! window.

use super::diag::Diag;
use super::identity::generated::crucible_window_identity_manager_v1::CrucibleWindowIdentityManagerV1;
use super::identity::generated::crucible_window_identity_v1::{
    CrucibleWindowIdentityV1, Event as IdentityEvent,
};
use super::identity::{OpenSteps, open_in_order};
use super::input::{self, SeatInput};
use super::routing::{Routes, TermId};
use super::term::{Scale, Surf, Term};
use super::{Config, WindowError, lifecycle, wl};
use crate::font::FontPool;
use crate::outbox::Outbox;
use crate::session_life;
use crucible_term_core::Session;
use crucible_term_core::pty::{Pty, SpawnSpec};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::TimeoutAction;
use smithay_client_toolkit::reexports::calloop::{
    Dispatcher, EventLoop, EventSource, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::csd_frame::WindowState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::ThemeSpec;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowHandler};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_shm,
    delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
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

mod steps;
use steps::RealSteps;

/// The connection-bound half of the host: registry, globals, seat, `Shm` and the identity
/// manager, plus the connection's calloop source. Dropped whole when the connection is lost
/// (daemon mode) and rebuilt by [`Host::reconnect`].
pub(crate) struct Wl {
    pub(super) registry: RegistryState,
    pub(super) seat: SeatState,
    pub(super) output: OutputState,
    pub(super) shm: Shm,
    pub(super) compositor: CompositorState,
    pub(super) xdg: XdgShell,
    pub(super) viewporter: Option<WpViewporter>,
    pub(super) fractional_mgr: Option<WpFractionalScaleManagerV1>,
    /// `crucible_window_identity_manager_v1`; used only by the daemon's granted opens.
    pub(super) identity: Option<CrucibleWindowIdentityManagerV1>,
    pub(super) qh: QueueHandle<Host>,
    /// The connection's `WaylandSource` in the loop (removed on loss, see [`Registered`]).
    source: Registered<WaylandSource<Host>>,
    /// Host-wide pool for the 1×1 placeholder buffers suspended windows show (created lazily on
    /// the first suspend, shared by all windows).
    pub(super) tiny_pool: Option<SlotPool>,
}

/// A calloop source registered through a [`Dispatcher`] we keep a clone of, so removing it
/// never drops the source inside calloop's `LoopHandle::remove`. `remove` holds the loop's
/// source table borrowed while it drops its own reference; if that were the last one, a
/// `Drop` that itself calls `remove` would panic with "RefCell already borrowed". The
/// `WaylandSource` is such a source: it owns the connection's last reference, and dropping
/// that drops sctk's keyboard `RepeatData`, whose `Drop` removes its armed key-repeat timer
/// (a key held when the compositor exits).
pub(crate) struct Registered<S: EventSource + 'static> {
    token: RegistrationToken,
    dispatcher: Dispatcher<'static, S, Host>,
}

impl<S: EventSource + 'static> Registered<S> {
    pub(crate) fn insert<F>(
        handle: &LoopHandle<'static, Host>,
        source: S,
        callback: F,
    ) -> smithay_client_toolkit::reexports::calloop::Result<Self>
    where
        F: FnMut(S::Event, &mut S::Metadata, &mut Host) -> S::Ret + 'static,
    {
        let dispatcher = Dispatcher::new(source, callback);
        let token = handle.register_dispatcher(dispatcher.clone())?;
        Ok(Registered { token, dispatcher })
    }

    /// Unregister, then drop the source after calloop has released its borrow.
    pub(crate) fn remove(self, handle: &LoopHandle<'static, Host>) {
        handle.remove(self.token);
        drop(self.dispatcher);
    }
}

/// The process's windows and everything they share; the calloop event loop's data. The
/// sessions (each [`Term`]'s PTY, pidfd and `Session`) are owned here independently of the
/// optional Wayland half, so a daemon survives the loss of its connection.
pub struct Host {
    /// The Wayland half; `None` while detached.
    pub(super) wl: Option<Wl>,
    pub(super) handle: LoopHandle<'static, Host>,
    /// Seat-wide input: devices, modifiers, serial, clipboard sources (empty while detached).
    pub(super) seat_in: SeatInput,
    pub(super) fonts: FontPool,
    pub(super) terms: BTreeMap<TermId, Term>,
    pub(super) routes: Routes<ObjectId>,
    next_id: u64,
    /// Exit codes of windows that are gone.
    exits: BTreeMap<TermId, i32>,
    /// Closed windows whose child is still in its SIGHUP grace.
    closing: usize,
    /// Daemon mode: a lost connection detaches the sessions. Standalone: the loop ends.
    detachable: bool,
    /// The display socket captured at start; reconnects go there.
    display: Option<PathBuf>,
}

/// The display socket libwayland would connect to: `$WAYLAND_DISPLAY` (default `wayland-0`)
/// under `$XDG_RUNTIME_DIR`, or itself when absolute.
pub(crate) fn display_path(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let name = PathBuf::from(env("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into()));
    if name.is_absolute() {
        return Some(name);
    }
    Some(PathBuf::from(env("XDG_RUNTIME_DIR")?).join(name))
}

fn lp(e: impl std::fmt::Display) -> WindowError {
    WindowError::Loop(e.to_string())
}

/// Spawn `argv` on a PTY (child environment `env`) in a new window and run until the child exits
/// or the window closes. Returns the child's exit code (128 + signal when killed). A lost
/// connection ends the loop with an error (standalone never detaches).
pub fn run(
    cfg: &Config,
    argv: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
) -> Result<i32, WindowError> {
    let (mut event_loop, mut host) = Host::new(false)?;
    let (id, _) = host.open_window(cfg, argv, env, None)?;
    while !host.terms.is_empty() || host.closing > 0 {
        event_loop.dispatch(None, &mut host).map_err(lp)?;
        host.reap();
    }
    Ok(host.exits.get(&id).copied().unwrap_or(0))
}

impl Dispatch<CrucibleWindowIdentityManagerV1, ()> for Host {
    fn event(
        _: &mut Self,
        _: &CrucibleWindowIdentityManagerV1,
        _: <CrucibleWindowIdentityManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<CrucibleWindowIdentityV1, ()> for Host {
    fn event(
        _: &mut Self,
        proxy: &CrucibleWindowIdentityV1,
        event: IdentityEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The window stays mapped either way (CDE treats an unbound one as an App).
        if let IdentityEvent::Rejected { reason } = event {
            eprintln!("crucible-term: window identity rejected: {reason}");
        }
        proxy.destroy();
    }
}

impl Wl {
    /// Bind the globals on `conn` and insert its `WaylandSource` into the loop. Also returns the
    /// connection's seat input.
    fn connect(
        conn: Connection,
        handle: &LoopHandle<'static, Host>,
    ) -> Result<(Wl, SeatInput), WindowError> {
        let (globals, queue) = registry_queue_init::<Host>(&conn).map_err(wl)?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).map_err(wl)?;
        let xdg = XdgShell::bind(&globals, &qh).map_err(wl)?;
        let shm = Shm::bind(&globals, &qh).map_err(wl)?;
        let viewporter = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok();
        let fractional_mgr = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
            .ok();
        let identity = globals
            .bind::<CrucibleWindowIdentityManagerV1, _, _>(&qh, 1..=1, ())
            .ok();
        let mut seat_in = SeatInput::new(&globals, &qh);
        let registry = RegistryState::new(&globals);
        let seat = SeatState::new(&globals, &qh);
        // sctk calls `new_seat` only for seats announced after this point; the seats already in
        // the registry (in practice, every seat) need their data and primary-selection devices
        // bound here, or copy and paste silently do nothing.
        if let Some(s) = seat.seats().next() {
            seat_in.bind_devices(&qh, &s);
        }
        let output = OutputState::new(&globals, &qh);
        let source = Registered::insert(handle, WaylandSource::new(conn, queue), |_, q, h| {
            q.dispatch_pending(h)
        })
        .map_err(lp)?;
        Ok((
            Wl {
                registry,
                seat,
                output,
                shm,
                compositor,
                xdg,
                viewporter,
                fractional_mgr,
                identity,
                qh,
                source,
                tiny_pool: None,
            },
            seat_in,
        ))
    }
}

impl Host {
    /// Connect to Wayland and bind the globals. The host stays resident with zero windows; the
    /// caller drives the returned loop (the daemon inserts its own sources through
    /// [`EventLoop::handle`]) and calls [`Host::reap`] after each dispatch. `detachable`
    /// (daemon mode): a lost connection detaches the sessions ([`Host::detach`]) and
    /// [`Host::reconnect`] retries the display captured here.
    pub(crate) fn new(detachable: bool) -> Result<(EventLoop<'static, Host>, Host), WindowError> {
        let conn = Connection::connect_to_env().map_err(wl)?;
        let event_loop: EventLoop<Host> = EventLoop::try_new().map_err(lp)?;
        let handle = event_loop.handle();
        let (w, seat_in) = Wl::connect(conn, &handle)?;
        let mut host = Host::detached(handle, detachable, display_path(&|k| std::env::var_os(k)));
        host.wl = Some(w);
        host.seat_in = seat_in;
        Ok((event_loop, host))
    }

    /// A host with no connection (the detached state; also the compositor-free test host).
    fn detached(
        handle: LoopHandle<'static, Host>,
        detachable: bool,
        display: Option<PathBuf>,
    ) -> Host {
        Host {
            wl: None,
            handle,
            seat_in: SeatInput::detached(),
            fonts: FontPool::default(),
            terms: BTreeMap::new(),
            routes: Routes::default(),
            next_id: 1,
            exits: BTreeMap::new(),
            closing: 0,
            detachable,
            display,
        }
    }

    /// The connection is gone: drop every Wayland object (surfaces, seat, globals, the source)
    /// and keep every session (PTY source, pidfd, `Session`), which goes on parsing output.
    /// Returns false (nothing dropped) when the host is not detachable (standalone).
    pub(crate) fn detach(&mut self) -> bool {
        if !self.detachable {
            return false;
        }
        for t in self.terms.values_mut() {
            if let Some(s) = t.surf.take() {
                s.teardown();
            }
            // Unfocused: the blink timer stops instead of ticking for no surface.
            t.focused = false;
        }
        self.routes = Routes::default();
        self.seat_in = SeatInput::detached();
        if let Some(w) = self.wl.take() {
            let Wl { source, .. } = w;
            source.remove(&self.handle);
        }
        eprintln!(
            "crucible-term: Wayland connection lost; {} session(s) detached",
            self.terms.len()
        );
        true
    }

    /// No Wayland connection.
    pub(crate) fn is_lost(&self) -> bool {
        self.wl.is_none()
    }

    /// Session `id` has no surface (detached, or not rebound since a reconnect).
    pub(crate) fn is_detached(&self, id: TermId) -> bool {
        self.terms.get(&id).is_some_and(|t| t.surf.is_none())
    }

    /// Connected, or a new connection to the captured display was established now. Detached
    /// sessions stay surface-less until [`Host::attach`] binds them.
    pub(crate) fn reconnect(&mut self) -> bool {
        if self.wl.is_some() {
            return true;
        }
        let Some(path) = self.display.clone() else {
            return false;
        };
        let conn = match std::os::unix::net::UnixStream::connect(&path) {
            Ok(s) => match Connection::from_socket(s) {
                Ok(c) => c,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        match Wl::connect(conn, &self.handle) {
            Ok((w, seat_in)) => {
                self.wl = Some(w);
                self.seat_in = seat_in;
                eprintln!(
                    "crucible-term: Wayland reconnected to {}; {} session(s) detached",
                    path.display(),
                    self.terms.len()
                );
                true
            }
            Err(e) => {
                eprintln!("crucible-term: Wayland reconnect: {e}");
                false
            }
        }
    }

    /// Open one toplevel running `argv` with environment `env` on a new PTY; returns the window
    /// id and the child (root) pid. Standalone opens exactly one.
    pub(crate) fn open_window(
        &mut self,
        cfg: &Config,
        argv: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        grant: Option<String>,
    ) -> Result<(TermId, u32), WindowError> {
        let w = self
            .wl
            .as_ref()
            .ok_or_else(|| WindowError::Wayland("no Wayland connection".into()))?;
        let id = TermId(self.next_id);
        self.next_id += 1;
        // A grant without the global cannot bind: open as an ungranted window.
        let grant = grant.filter(|_| w.identity.is_some());
        let mut steps = RealSteps {
            wl: w,
            id,
            spec: Some(SpawnSpec {
                argv,
                cwd: cfg.cwd.clone(),
                env,
                size: (cfg.cols, cfg.rows),
            }),
            viewport: None,
            fractional: None,
            surface_id: None,
        };
        let (early_pty, window) =
            open_in_order(&mut steps, grant.as_deref()).map_err(WindowError::Pty)?;
        let RealSteps {
            spec,
            viewport,
            fractional,
            surface_id,
            ..
        } = steps;
        let pty = match early_pty {
            Some(pty) => pty,
            None => Pty::spawn(&spec.expect("spawn spec unused")).map_err(WindowError::Pty)?,
        };
        let mut term = self.new_session(id, cfg, pty)?;
        Self::bind_surface(
            self.wl.as_ref().expect("connected above"),
            &mut self.routes,
            &mut term,
            window,
            viewport,
            fractional,
            surface_id.expect("role step ran"),
        )?;
        let root_pid = term.pty.pid();
        self.terms.insert(id, term);
        Ok((id, root_pid))
    }

    /// Bind the freshly created toplevel `window` (and its viewport / fractional-scale objects)
    /// to the surface-less `term`: title, app id, first commit, the shm pool sized from
    /// `term.logical`, the scale mode and the route from `surface_id`. Shared by
    /// [`Host::open_window`] and [`Host::attach`].
    fn bind_surface(
        w: &Wl,
        routes: &mut Routes<ObjectId>,
        term: &mut Term,
        window: Window,
        viewport: Option<WpViewport>,
        fractional: Option<WpFractionalScaleV1>,
        surface_id: ObjectId,
    ) -> Result<(), WindowError> {
        window.set_title(term.cfg.title.clone());
        window.set_app_id(term.cfg.app_id.clone());
        window.commit();
        let pool =
            SlotPool::new((term.logical.0 * term.logical.1 * 4) as usize, &w.shm).map_err(wl)?;
        term.scale = term.scale.for_surface(fractional.is_some());
        term.surf = Some(Surf::new(pool, window, viewport, fractional, w.qh.clone()));
        routes.insert(surface_id, term.id);
        Ok(())
    }

    /// The session half of window `id` on `pty`: the PTY and pidfd sources and the `Session`,
    /// with no surface.
    fn new_session(&mut self, id: TermId, cfg: &Config, pty: Pty) -> Result<Term, WindowError> {
        let fonts = self
            .fonts
            .get(&cfg.settings.family, cfg.settings.size_pt, 1.0)
            .map_err(WindowError::Font)?;
        let m = fonts.fonts.borrow().metrics();
        let logical = (
            u32::from(cfg.cols) * m.cell_w + 2 * cfg.settings.pad.0,
            u32::from(cfg.rows) * m.cell_h + 2 * cfg.settings.pad.1,
        );
        let master = pty.master().try_clone().map_err(lp)?;
        // Non-blocking so one wakeup drains everything the child wrote.
        rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK).map_err(lp)?;
        let mut sources = Vec::new();
        // The PTY source belongs to the session, not the surface: it keeps draining (and the
        // session keeps parsing) while detached, so the child never blocks on a full PTY.
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
                        let gone = t.exit.is_some();
                        host.refresh_cursor(id);
                        Ok(if gone {
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
        sources.extend(
            session_life::watch_exit(&self.handle, pty.pid(), move |host: &mut Host| {
                let focus = host.routes.holds_focus(id);
                if let Some(t) = host.terms.get_mut(&id) {
                    t.on_child_exited(&mut host.seat_in, focus);
                }
            })
            .map_err(lp)?,
        );

        let mut term = Term {
            id,
            surf: None,
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
            scale: Scale::Integer(1),
            buf: (0, 0),
            dirty: false,
            full: true,
            focused: true,
            blink_on: true,
            blink_armed: false,
            sync_since: None,
            sync_timer: false,
            last_cursor: None,
            exit: None,
            close_requested: false,
            diag: Diag::from_env(),
            force_present: false,
            out_gen: 0,
            last_output: Instant::now(),
        };
        term.apply_cursor_defaults();
        if term.diag.mem() {
            term.add_timer(Duration::from_secs(4), |t, _| {
                t.print_mem(t.surf.as_ref(), "idle-4s");
                TimeoutAction::Drop
            });
        }
        Ok(term)
    }

    /// Bind a new surface to the existing, surface-less session `id` (the reattach after a
    /// loss), binding the window identity with `grant` when the global is advertised. Returns
    /// the session's root pid. The session's grid and scrollback carry over; the new surface
    /// repaints in full on its first configure.
    pub(crate) fn attach(&mut self, id: TermId, grant: Option<String>) -> Result<u32, WindowError> {
        let w = self
            .wl
            .as_ref()
            .ok_or_else(|| WindowError::Wayland("no Wayland connection".into()))?;
        let t = self
            .terms
            .get_mut(&id)
            .filter(|t| t.exit.is_none() && !t.close_requested)
            .ok_or_else(|| WindowError::Wayland(format!("no session {}", id.0)))?;
        if t.surf.is_some() {
            return Err(WindowError::Wayland(format!("session {} is bound", id.0)));
        }
        let root_pid = t.pty.pid();
        let mut steps = RealSteps {
            wl: w,
            id,
            spec: None,
            viewport: None,
            fractional: None,
            surface_id: None,
        };
        let surface = steps.create_surface();
        if let Some(g) = grant.filter(|_| w.identity.is_some()) {
            steps.bind_surface(&surface, &g, root_pid);
        }
        let window = steps.create_role(surface);
        let RealSteps {
            viewport,
            fractional,
            surface_id,
            ..
        } = steps;
        Self::bind_surface(
            w,
            &mut self.routes,
            t,
            window,
            viewport,
            fractional,
            surface_id.expect("role step ran"),
        )?;
        t.buf = (0, 0);
        t.full = true;
        t.dirty = true;
        t.focused = true;
        Ok(root_pid)
    }

    /// Session `id`'s VT state and output activity (the daemon's record capture).
    pub(crate) fn view(&self, id: TermId) -> Option<crate::daemon::store::View<'_>> {
        self.terms.get(&id).map(|t| crate::daemon::store::View {
            session: &t.session,
            generation: t.out_gen,
            last_output: t.last_output,
            finishing: t.exit.is_some() || t.close_requested,
        })
    }

    /// Replace window `id`'s session, before any PTY output, with `rec` replayed at the
    /// session's size; returns the restored row count and the output generation.
    pub(crate) fn restore(
        &mut self,
        id: TermId,
        rec: &crate::daemon::store::Record,
    ) -> Option<(usize, u64)> {
        let t = self.terms.get_mut(&id)?;
        let (cols, rows) = t.session.size();
        let (s, n) = crate::daemon::store::restored(rec, cols, rows, t.cfg.settings.scrollback);
        t.session = s;
        t.apply_cursor_defaults();
        t.full = true;
        t.dirty = true;
        Some((n, t.out_gen))
    }

    /// Whether the compositor advertises the window-identity global.
    pub(crate) fn has_identity(&self) -> bool {
        self.wl.as_ref().is_some_and(|w| w.identity.is_some())
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
    fn begin_grace(&mut self, id: TermId, pty: Pty) {
        self.closing += 1;
        let armed = session_life::begin_grace(&self.handle, pty, move |host: &mut Host, code| {
            host.exits.insert(id, code);
            host.closing -= 1;
        });
        if !armed {
            self.closing -= 1;
            self.exits.insert(id, 128 + 1);
        }
    }

    /// The Wayland half, for sctk's state accessors. They are only called while dispatching the
    /// connection's queue, which exists only while the half does (its source is removed with it).
    fn live(&mut self) -> &mut Wl {
        self.wl
            .as_mut()
            .expect("sctk state is only reached from a live connection's dispatch")
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
        if let Scale::Integer(cur) = t.scale
            && factor != cur
        {
            t.scale = Scale::Integer(factor.max(1));
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
            if let Some(s) = t.surf.as_mut() {
                s.frame_pending = false;
            }
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
        let (Some(t), Some(w)) = (self.terms.get_mut(&id), self.wl.as_mut()) else {
            return;
        };
        let Some(s) = t.surf.as_mut() else {
            return;
        };
        s.configured = true;
        let transition = s
            .life
            .transition(configure.state.contains(WindowState::SUSPENDED));
        if let (Some(wd), Some(h)) = configure.new_size {
            t.logical = (wd.get(), h.get());
        }
        let focused = configure.state.contains(WindowState::ACTIVATED);
        if focused != t.focused {
            t.focused = focused;
            t.arm_blink();
        }
        // `suspended` transitions (never taken on a compositor that does not send it). sctk has
        // already acked this configure, so the commits below follow the ack. Suspend runs before
        // the relayout (whose draw is then a no-op); resume clears the stale frame-pending
        // before it, so the relayout's draw repaints and commits the whole grid right here.
        match transition {
            lifecycle::Transition::Suspend => {
                if w.tiny_pool.is_none() {
                    match SlotPool::new(4096, &w.shm) {
                        Ok(p) => w.tiny_pool = Some(p),
                        Err(e) => eprintln!("crucible-term: placeholder pool: {e}"),
                    }
                }
                if let Some(tiny) = w.tiny_pool.as_mut() {
                    t.suspend(tiny, &w.shm);
                }
            }
            lifecycle::Transition::Resume => t.resume(),
            lifecycle::Transition::None => {}
        }
        t.relayout(&mut self.fonts);
    }
}

impl OutputHandler for Host {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.live().output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for Host {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.live().seat
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
                let Some(w) = self.wl.as_mut() else {
                    return;
                };
                let kbd = w.seat.get_keyboard_with_repeat(
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
                let Some(w) = self.wl.as_mut() else {
                    return;
                };
                let surface = w.compositor.create_surface(qh);
                match w.seat.get_pointer_with_theme(
                    qh,
                    &seat,
                    w.shm.wl_shm(),
                    surface,
                    ThemeSpec::System,
                ) {
                    Ok(p) => self.seat_in.bind_pointer(p),
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
                // Dropping the themed pointer destroys the shape device, releases the pointer
                // and destroys the cursor surface (no `release()` here: that would double it).
                self.seat_in.unbind_pointer();
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for Host {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.live().shm
    }
}

impl ProvidesRegistryState for Host {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.live().registry
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
            // A fractional-scale object exists only on a fractional surface.
            if let Scale::Fractional(cur) = t.scale
                && (s - cur).abs() > f64::EPSILON
            {
                t.scale = Scale::Fractional(s);
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

/// The detach path without a compositor: a [`Host`] whose Wayland half is absent owns sessions
/// exactly as a detached daemon does.
#[cfg(test)]
mod tests;

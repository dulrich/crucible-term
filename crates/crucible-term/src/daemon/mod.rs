//! `cterm daemon`: the per-user window daemon. It listens on the per-display socket
//! ([`paths`]), checks each peer's UID (`SO_PEERCRED`), answers `hello`, and turns each
//! client's `open` into a window on its backend (the Wayland [`Host`], or the test-only
//! [`headless`] one), relaying `exited` when the child ends. `close` or EOF from the client
//! closes the window through the SIGHUP grace. It stays resident after its last window
//! (`daemon-linger N` exits after N idle seconds). When the Wayland connection is lost it
//! *detaches*: every session (PTY, child, `Session`) stays live and keeps draining its PTY, a
//! client EOF no longer closes it, a child exit is still relayed, and the display it started on
//! is retried every second ([`RECONNECT`]). An `open` while detached first tries the display
//! once and is refused if it is still gone.
//!
//! State (connections, window → connection) is held in an `Rc<RefCell<State>>` captured by the
//! calloop sources, so the window host carries no daemon fields.

pub mod headless;
pub mod paths;

use crate::client::{GRANT_VAR, strip_grant};
use crate::config;
use crate::window::identity;
use crate::window::{self, Host, TermId};
use crate::wire::{self, Msg, Open};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Env var naming the readiness pipe's write fd (set by the starting client).
pub const READY_FD_VAR: &str = "CTERM_DAEMON_READY_FD";
/// Env var asking `cterm daemon` to detach (the first stage of the double fork).
pub const DETACH_VAR: &str = "CTERM_DAEMON_DETACH";
/// TEST-ONLY: `1` runs the daemon without Wayland ([`headless`]).
pub const HEADLESS_VAR: &str = "CTERM_DEBUG_DAEMON_HEADLESS";
/// TEST-ONLY: N > 0 defers the first granted `open` by N ms (on a calloop timer, never a sleep).
pub const OPEN_DELAY_VAR: &str = "CTERM_DEBUG_OPEN_DELAY_MS";
/// Retry period of the reconnect after a Wayland loss.
const RECONNECT: Duration = Duration::from_secs(1);

/// What the daemon needs from a window backend.
pub trait Backend: 'static {
    /// Open a window running `argv` with `env`; returns the window id and the child pid.
    fn open(
        &mut self,
        cfg: &window::Config,
        argv: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        grant: Option<String>,
    ) -> Result<(u64, u32), String>;
    /// Whether this backend can bind a window identity (the compositor advertises the global).
    fn has_identity(&self) -> bool {
        false
    }
    /// Close window `id` through the SIGHUP grace.
    fn close(&mut self, id: u64);
    /// Remove finished windows; called after every dispatch.
    fn reap(&mut self);
    /// Exit codes recorded since the last call.
    fn drain_exits(&mut self) -> Vec<(u64, i32)>;
    /// No window and no child in its grace.
    fn is_idle(&self) -> bool;
    /// A dispatch timeout the backend needs while it polls (none: block).
    fn poll_hint(&self) -> Option<Duration> {
        None
    }
    /// The loop failed (the Wayland connection is lost): detach every session and return true,
    /// or return false to end the daemon.
    fn on_lost(&mut self) -> bool {
        false
    }
    /// No Wayland connection (detached).
    fn is_lost(&self) -> bool {
        false
    }
    /// Connected, or reconnected now to the display the daemon started on.
    fn reconnect(&mut self) -> bool {
        true
    }
    /// Window `id` has no surface (its session survives a client EOF).
    fn is_detached(&self, _id: u64) -> bool {
        false
    }
    /// Bind a new surface to the existing, surface-less session `id` (binding the window
    /// identity with `grant`); returns its root pid.
    // WP-2 calls this from the keyed `open`; until then only tests do.
    #[allow(dead_code)]
    fn attach(&mut self, id: u64, grant: Option<String>) -> Result<u32, String>;
}

impl Backend for Host {
    fn open(
        &mut self,
        cfg: &window::Config,
        argv: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        grant: Option<String>,
    ) -> Result<(u64, u32), String> {
        self.open_window(cfg, argv, env, grant)
            .map(|(id, pid)| (id.0, pid))
            .map_err(|e| e.to_string())
    }
    fn has_identity(&self) -> bool {
        self.has_identity()
    }
    fn close(&mut self, id: u64) {
        Host::close(self, TermId(id));
    }
    fn reap(&mut self) {
        Host::reap(self);
    }
    fn drain_exits(&mut self) -> Vec<(u64, i32)> {
        Host::drain_exits(self)
            .into_iter()
            .map(|(id, c)| (id.0, c))
            .collect()
    }
    fn is_idle(&self) -> bool {
        Host::is_idle(self)
    }
    fn on_lost(&mut self) -> bool {
        self.detach()
    }
    fn is_lost(&self) -> bool {
        Host::is_lost(self)
    }
    fn reconnect(&mut self) -> bool {
        Host::reconnect(self)
    }
    fn is_detached(&self, id: u64) -> bool {
        Host::is_detached(self, TermId(id))
    }
    fn attach(&mut self, id: u64, grant: Option<String>) -> Result<u32, String> {
        Host::attach(self, TermId(id), grant).map_err(|e| e.to_string())
    }
}

/// The peer-UID rule: only our own user may talk to the daemon.
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
    hello: bool,
    term: Option<u64>,
    /// An `open` is waiting on the test-only delay timer.
    opening: bool,
    token: Option<RegistrationToken>,
}

#[derive(Default)]
struct State {
    conns: BTreeMap<u64, Conn>,
    by_term: BTreeMap<u64, u64>,
    next_conn: u64,
    /// TEST-ONLY delay still to apply to the first granted `open`.
    delay: Option<Duration>,
    /// A deferred open to schedule: set by `on_msg`, armed by the readable callback.
    armed: Option<(u64, Duration, Open)>,
}

/// TEST-ONLY: parse `CTERM_DEBUG_OPEN_DELAY_MS`: unset, `0` or unparsable mean no delay.
pub fn parse_open_delay(v: Option<&str>) -> Option<Duration> {
    let n: u64 = v?.trim().parse().ok()?;
    (n > 0).then(|| Duration::from_millis(n))
}

/// Which `open` is delayed: the first granted one (consumes the slot); ungranted opens never are.
fn take_delay(slot: &mut Option<Duration>, granted: bool) -> Option<Duration> {
    if granted { slot.take() } else { None }
}

impl State {
    fn send(&mut self, conn: u64, m: &Msg) {
        if let Some(c) = self.conns.get_mut(&conn) {
            let _ = c.stream.write_all(&wire::encode(m));
        }
    }
}

/// Build the window config for an `open`: `config.kdl` re-read from the path the client's env
/// names, the client's overrides on top, then its cwd, app id and title.
fn open_config(o: &Open) -> window::Config {
    let lookup = |k: &str| o.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let path = config::config_path(&lookup);
    let (settings, warnings) = config::load(
        path.as_deref(),
        &|p| std::fs::read_to_string(p),
        &o.overrides,
    );
    for w in warnings {
        eprintln!("{w}");
    }
    let mut cfg = window::Config {
        settings,
        cwd: o.cwd.clone(),
        ..window::Config::default()
    };
    if let Some(id) = &o.app_id {
        cfg.app_id = id.clone();
    }
    if let Some(t) = &o.title {
        cfg.title = t.clone();
    }
    cfg
}

/// Run an accepted `open`: build the config, open the window, answer the client.
fn do_open<B: Backend>(st: &mut State, b: &mut B, conn: u64, o: Open, granted: bool) {
    let cfg = open_config(&o);
    let grant = if granted { o.grant } else { None };
    match b.open(&cfg, o.argv, strip_grant(o.env), grant) {
        Ok((id, root_pid)) => {
            if let Some(c) = st.conns.get_mut(&conn) {
                c.term = Some(id);
            }
            st.by_term.insert(id, conn);
            st.send(conn, &Msg::Opened { root_pid });
        }
        Err(reason) => {
            eprintln!("crucible-term: daemon: open: {reason}");
            st.send(conn, &Msg::Refused { reason });
        }
    }
}

/// Handle one decoded message from `conn`. Returns false to drop the connection.
fn on_msg<B: Backend>(st: &mut State, b: &mut B, conn: u64, m: Msg) -> bool {
    let Some(c) = st.conns.get_mut(&conn) else {
        return false;
    };
    if !c.hello {
        match wire::judge_hello(&m, env!("FLEET_BUILD_ID")) {
            wire::HelloVerdict::Fallback => {
                eprintln!("crucible-term: daemon: client {conn}: protocol mismatch ({m:?})");
            }
            wire::HelloVerdict::BuildMismatch => {
                eprintln!("crucible-term: daemon: client {conn}: build id differs, serving")
            }
            wire::HelloVerdict::Ok => {}
        }
        c.hello = true;
        return matches!(m, Msg::Hello { .. });
    }
    match m {
        Msg::Open(o) => {
            if c.term.is_some() || c.opening {
                st.send(
                    conn,
                    &Msg::Refused {
                        reason: "already open".into(),
                    },
                );
                return true;
            }
            // Detached: try the display once, synchronously; still lost refuses so the client
            // falls back to standalone. The client falls back only on `no-identity` until WP-2
            // adds `no-wayland`, so that is the reason sent.
            if b.is_lost() && !b.reconnect() {
                st.send(
                    conn,
                    &Msg::Refused {
                        reason: "no-identity".into(),
                    },
                );
                return true;
            }
            let decision = identity::decide(o.grant.is_some(), b.has_identity());
            if decision == identity::OpenDecision::Refuse {
                // No identity global: a granted spawn falls back to standalone.
                st.send(
                    conn,
                    &Msg::Refused {
                        reason: "no-identity".into(),
                    },
                );
                return true;
            }
            let granted = decision == identity::OpenDecision::OpenBound;
            if let Some(d) = take_delay(&mut st.delay, granted) {
                eprintln!(
                    "crucible-term: daemon: client {conn}: open deferred {} ms (CTERM_DEBUG_OPEN_DELAY_MS)",
                    d.as_millis()
                );
                if let Some(c) = st.conns.get_mut(&conn) {
                    c.opening = true;
                }
                st.armed = Some((conn, d, o));
                return true;
            }
            do_open(st, b, conn, o, granted);
            true
        }
        Msg::Close => {
            if let Some(id) = c.term {
                b.close(id);
            }
            true
        }
        other => {
            eprintln!("crucible-term: daemon: client {conn}: unexpected {other:?}");
            false
        }
    }
}

/// Drop `conn`: its window (if still open) closes through the SIGHUP grace, unless it is
/// detached: a detached session outlives its client (a later rebind reattaches it).
fn drop_conn<B: Backend>(st: &mut State, b: &mut B, conn: u64) {
    if let Some(c) = st.conns.remove(&conn)
        && let Some(id) = c.term
        && st.by_term.remove(&id).is_some()
        && !b.is_detached(id)
    {
        b.close(id);
    }
}

fn on_readable<B: Backend>(st: &mut State, b: &mut B, conn: u64) -> PostAction {
    let mut eof = false;
    if let Some(c) = st.conns.get_mut(&conn) {
        let mut chunk = [0u8; 8192];
        loop {
            match (&c.stream).read(&mut chunk) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => c.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    eof = true;
                    break;
                }
            }
        }
    } else {
        return PostAction::Remove;
    }
    loop {
        let Some(c) = st.conns.get_mut(&conn) else {
            return PostAction::Remove;
        };
        match wire::decode(&c.buf) {
            Ok(Some((m, n))) => {
                c.buf.drain(..n);
                if !on_msg(st, b, conn, m) {
                    eof = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("crucible-term: daemon: client {conn}: {e}");
                eof = true;
                break;
            }
        }
    }
    if eof {
        drop_conn(st, b, conn);
        PostAction::Remove
    } else {
        PostAction::Continue
    }
}

/// TEST-ONLY: run the deferred `open` of `conn` after `d`, on a calloop timer (the loop keeps serving).
fn arm_deferred_open<B: Backend>(
    state: &Rc<RefCell<State>>,
    handle: &LoopHandle<'static, B>,
    conn: u64,
    d: Duration,
    o: Open,
) {
    let state = state.clone();
    let mut o = Some(o);
    let r = handle.insert_source(Timer::from_duration(d), move |_, _, b: &mut B| {
        let mut st = state.borrow_mut();
        let alive = st.conns.get_mut(&conn).is_some_and(|c| {
            let was = c.opening;
            c.opening = false;
            was
        });
        if let Some(o) = o.take()
            && alive
        {
            do_open(&mut st, b, conn, o, true);
        }
        TimeoutAction::Drop
    });
    if let Err(e) = r {
        eprintln!("crucible-term: daemon: open delay timer: {e}");
    }
}

fn on_accept<B: Backend>(
    state: &Rc<RefCell<State>>,
    handle: &LoopHandle<'static, B>,
    listener: &UnixListener,
    own_uid: u32,
) {
    loop {
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let peer = rustix::net::sockopt::socket_peercred(&stream).map(|c| c.uid.as_raw());
        match peer {
            Ok(uid) if peer_allowed(uid, own_uid) => {}
            Ok(uid) => {
                eprintln!("crucible-term: daemon: rejected peer uid {uid} (own uid {own_uid})");
                continue;
            }
            Err(e) => {
                eprintln!("crucible-term: daemon: SO_PEERCRED: {e}; rejected");
                continue;
            }
        }
        if stream.set_nonblocking(true).is_err() {
            continue;
        }
        let Ok(fd) = rustix::io::dup(&stream) else {
            continue;
        };
        let mut st = state.borrow_mut();
        let conn = st.next_conn;
        st.next_conn += 1;
        let _ = stream.write_all(&wire::encode(&Msg::Hello {
            protocol_version: wire::PROTOCOL_VERSION,
            build_id: env!("FLEET_BUILD_ID").into(),
        }));
        st.conns.insert(
            conn,
            Conn {
                stream,
                buf: Vec::new(),
                hello: false,
                term: None,
                opening: false,
                token: None,
            },
        );
        drop(st);
        let s2 = state.clone();
        let h2 = handle.clone();
        let tok = handle.insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, _, b: &mut B| {
                let post = on_readable(&mut s2.borrow_mut(), b, conn);
                let armed = s2.borrow_mut().armed.take();
                if let Some((c, d, o)) = armed {
                    arm_deferred_open(&s2, &h2, c, d, o);
                }
                Ok(post)
            },
        );
        let mut st = state.borrow_mut();
        match tok {
            Ok(t) => {
                if let Some(c) = st.conns.get_mut(&conn) {
                    c.token = Some(t);
                }
            }
            Err(_) => {
                st.conns.remove(&conn);
            }
        }
    }
}

/// Bind the listener: refuse if a live daemon answers, else unlink a stale socket and bind.
fn bind(p: &paths::Paths) -> Result<UnixListener, String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&p.dir)
        .map_err(|e| format!("{}: {e}", p.dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p.dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", p.dir.display()))?;
    if UnixStream::connect(&p.socket).is_ok() {
        return Err(format!("already running on {}", p.socket.display()));
    }
    let _ = std::fs::remove_file(&p.socket);
    let l = UnixListener::bind(&p.socket).map_err(|e| format!("{}: {e}", p.socket.display()))?;
    l.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(l)
}

/// The readiness pipe the starting client passed, if any.
fn ready_fd() -> Option<OwnedFd> {
    let n: i32 = std::env::var(READY_FD_VAR).ok()?.parse().ok()?;
    if n < 3 {
        return None;
    }
    #[allow(unsafe_code)]
    // SAFETY: the starting client created this fd for this process alone and named it in
    // `READY_FD_VAR`; nothing else here owns it. If it is not open, the fcntl below fails and
    // the value is forgotten without closing anything.
    let fd = unsafe { OwnedFd::from_raw_fd(n) };
    if rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).is_err() {
        std::mem::forget(fd);
        return None;
    }
    Some(fd)
}

/// First stage of the detach: re-exec `cterm daemon` in a new session and exit, so the client
/// reaps this stage at once and the daemon is left with no parent waiting on it.
fn detach() -> ExitCode {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("crucible-term: daemon: current_exe: {e}");
            return ExitCode::from(1);
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .env_remove(DETACH_VAR)
        .env_remove(GRANT_VAR);
    #[allow(unsafe_code)]
    // SAFETY: the closure runs between fork and exec and only calls setsid(2), which is
    // async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| rustix::process::setsid().map(|_| ()).map_err(Into::into));
    }
    match cmd.spawn() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crucible-term: daemon: spawn: {e}");
            ExitCode::from(1)
        }
    }
}

/// `cterm daemon`.
pub fn run() -> ExitCode {
    if std::env::var_os(DETACH_VAR).is_some() {
        return detach();
    }
    let env = |k: &str| std::env::var_os(k);
    let p = match paths::socket_paths(&env) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("crucible-term: daemon: {e}");
            return ExitCode::from(1);
        }
    };
    let (settings, warnings) = config::load(
        config::config_path(&env).as_deref(),
        &|p| std::fs::read_to_string(p),
        &[],
    );
    for w in warnings {
        eprintln!("{w}");
    }
    let ready = ready_fd();
    let listener = match bind(&p) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("crucible-term: daemon: {e}");
            return ExitCode::from(1);
        }
    };
    let linger = (settings.daemon_linger > 0).then(|| Duration::from_secs(settings.daemon_linger));
    let open_delay = parse_open_delay(
        env(OPEN_DELAY_VAR)
            .and_then(|v| v.into_string().ok())
            .as_deref(),
    );
    let headless = std::env::var_os(HEADLESS_VAR).is_some_and(|v| v == "1");
    eprintln!(
        "crucible-term: daemon: pid {} listening on {}{}",
        std::process::id(),
        p.socket.display(),
        if headless {
            " (headless test mode)"
        } else {
            ""
        }
    );
    let r = if headless {
        let event_loop = match EventLoop::<headless::Headless>::try_new() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("crucible-term: daemon: event loop: {e}");
                return ExitCode::from(1);
            }
        };
        let mut b = headless::Headless::new(event_loop.handle());
        let ms = |k: &str| {
            headless::Headless::parse_ms(env(k).and_then(|v| v.into_string().ok()).as_deref())
        };
        if let Some(loss) = ms(headless::LOSS_VAR)
            && let Err(e) = b.fake_wayland(loss, ms(headless::RECONNECT_VAR))
        {
            eprintln!("crucible-term: daemon: fake Wayland: {e}");
            return ExitCode::from(1);
        }
        serve(event_loop, b, listener, ready, linger, open_delay)
    } else {
        match Host::new(true) {
            Ok((event_loop, host)) => serve(event_loop, host, listener, ready, linger, open_delay),
            Err(e) => Err(e.to_string()),
        }
    };
    let _ = std::fs::remove_file(&p.socket);
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crucible-term: daemon: {e}; exiting");
            ExitCode::from(1)
        }
    }
}

/// Retry the display every [`RECONNECT`] until the backend is connected again.
fn arm_reconnect<B: Backend>(handle: &LoopHandle<'static, B>) {
    let r = handle.insert_source(Timer::from_duration(RECONNECT), |_, _, b: &mut B| {
        if b.reconnect() {
            eprintln!("crucible-term: daemon: reconnected");
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(RECONNECT)
        }
    });
    if let Err(e) = r {
        eprintln!("crucible-term: daemon: reconnect timer: {e}");
    }
}

fn serve<B: Backend>(
    mut event_loop: EventLoop<'static, B>,
    mut b: B,
    listener: UnixListener,
    ready: Option<OwnedFd>,
    linger: Option<Duration>,
    open_delay: Option<Duration>,
) -> Result<(), String> {
    let state = Rc::new(RefCell::new(State {
        delay: open_delay,
        ..State::default()
    }));
    let own_uid = rustix::process::getuid().as_raw();
    let handle = event_loop.handle();
    let lfd = rustix::io::dup(&listener).map_err(|e| e.to_string())?;
    {
        let state = state.clone();
        let h = handle.clone();
        handle
            .insert_source(
                Generic::new(lfd, Interest::READ, Mode::Level),
                move |_, _, _: &mut B| {
                    on_accept(&state, &h, &listener, own_uid);
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| e.to_string())?;
    }
    // Listening (and, in Wayland mode, connected): tell the starting client.
    if let Some(fd) = ready {
        let _ = rustix::io::write(&fd, b"r");
    }
    let mut idle_since = Instant::now();
    loop {
        let idle = b.is_idle() && state.borrow().conns.is_empty();
        let linger_left = match (linger, idle) {
            (Some(l), true) => {
                let left = l.saturating_sub(idle_since.elapsed());
                if left.is_zero() {
                    eprintln!("crucible-term: daemon: idle for {}s, exiting", l.as_secs());
                    return Ok(());
                }
                Some(left)
            }
            _ => {
                idle_since = Instant::now();
                None
            }
        };
        let timeout = match (b.poll_hint(), linger_left) {
            (Some(a), Some(c)) => Some(a.min(c)),
            (a, c) => a.or(c),
        };
        if let Err(e) = event_loop.dispatch(timeout, &mut b) {
            if !b.on_lost() {
                return Err(format!("event loop (Wayland connection lost?): {e}"));
            }
            eprintln!("crucible-term: daemon: {e}; detached, reconnecting");
            arm_reconnect(&handle);
        }
        b.reap();
        let exits = b.drain_exits();
        if !exits.is_empty() {
            let mut st = state.borrow_mut();
            for (id, code) in exits {
                let Some(conn) = st.by_term.remove(&id) else {
                    continue;
                };
                st.send(conn, &Msg::Exited { code });
                if let Some(c) = st.conns.remove(&conn)
                    && let Some(t) = c.token
                {
                    handle.remove(t);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_granted_open_is_delayed() {
        let mut slot = parse_open_delay(Some("800"));
        assert_eq!(slot, Some(Duration::from_millis(800)));
        assert_eq!(
            take_delay(&mut slot, false),
            None,
            "ungranted opens never wait"
        );
        assert_eq!(
            take_delay(&mut slot, true),
            Some(Duration::from_millis(800))
        );
        assert_eq!(
            take_delay(&mut slot, true),
            None,
            "later opens are not delayed"
        );
        for off in [None, Some("0"), Some("x"), Some("")] {
            assert_eq!(parse_open_delay(off), None);
        }
    }

    #[test]
    fn only_the_own_uid_is_served() {
        assert!(peer_allowed(1000, 1000));
        assert!(!peer_allowed(0, 1000));
        assert!(!peer_allowed(1001, 1000));
    }
}

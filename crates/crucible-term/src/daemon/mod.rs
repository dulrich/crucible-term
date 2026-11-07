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
mod state;

use crate::client::{GRANT_VAR, SESSION_VAR};
use crate::config;
use crate::spawn_env::strip_cde_vars;
use crate::window::identity;
use crate::window::{self, Host, TermId};
use crate::wire::{self, Msg, Open};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction,
};
use state::{Conn, Expiry, SessionRec, State, detached_clock};
use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Env var naming the readiness pipe's write fd (set by the starting client).
pub const READY_FD_VAR: &str = "CTERM_DAEMON_READY_FD";
/// How long a session whose client hung up waits before it closes, so a compositor quit that
/// takes the thin clients down with it detaches the session instead (see [`drop_conn`]).
const ORPHAN_GRACE: Duration = Duration::from_secs(2);

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
    ) -> Result<(TermId, u32), String>;
    /// Whether this backend can bind a window identity (the compositor advertises the global).
    fn has_identity(&self) -> bool {
        false
    }
    /// Close window `id` through the SIGHUP grace.
    fn close(&mut self, id: TermId);
    /// Remove finished windows; called after every dispatch.
    fn reap(&mut self);
    /// Exit codes recorded since the last call.
    fn drain_exits(&mut self) -> Vec<(TermId, i32)>;
    /// No window and no child in its grace.
    fn is_idle(&self) -> bool;
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
    fn is_detached(&self, _id: TermId) -> bool {
        false
    }
    /// Bind a new surface to the existing, surface-less session `id` (binding the window
    /// identity with `grant`); returns its root pid.
    fn attach(&mut self, id: TermId, grant: Option<String>) -> Result<u32, String>;
}

impl Backend for Host {
    fn open(
        &mut self,
        cfg: &window::Config,
        argv: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        grant: Option<String>,
    ) -> Result<(TermId, u32), String> {
        self.open_window(cfg, argv, env, grant)
            .map_err(|e| e.to_string())
    }
    fn has_identity(&self) -> bool {
        self.has_identity()
    }
    fn close(&mut self, id: TermId) {
        Host::close(self, id);
    }
    fn reap(&mut self) {
        Host::reap(self);
    }
    fn drain_exits(&mut self) -> Vec<(TermId, i32)> {
        Host::drain_exits(self)
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
    fn is_detached(&self, id: TermId) -> bool {
        Host::is_detached(self, id)
    }
    fn attach(&mut self, id: TermId, grant: Option<String>) -> Result<u32, String> {
        Host::attach(self, id, grant).map_err(|e| e.to_string())
    }
}

/// The peer-UID rule: only our own user may talk to the daemon.
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
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

/// Build the window config for an `open`: `config.kdl` re-read from the path the client's env
/// names, the client's overrides on top, then its cwd, app id and title.
fn open_config(o: &Open) -> window::Config {
    let lookup = |k: &str| o.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    config::window_config(
        &lookup,
        config::Spawn {
            overrides: &o.overrides,
            cwd: o.cwd.clone(),
            app_id: o.app_id.as_deref(),
            title: o.title.as_deref(),
        },
    )
}

/// Run an accepted `open`: build the config, open the window, answer the client.
fn do_open<B: Backend>(st: &mut State, b: &mut B, conn: u64, o: Open, granted: bool) {
    let cfg = open_config(&o);
    let grant = if granted { o.grant } else { None };
    let key = o.session_key;
    match b.open(&cfg, o.argv, strip_cde_vars(o.env), grant) {
        Ok((id, root_pid)) => {
            let mut rec = SessionRec {
                conn: Some(conn),
                ..SessionRec::default()
            };
            if let Some(k) = key {
                if !granted {
                    eprintln!(
                        "crucible-term: daemon: client {conn}: session key without a grant; session {id} is not reattachable"
                    );
                } else if let Some(old) = st.find(|r| r.key.as_deref() == Some(&k)) {
                    // Never steal: the key's session is still live (attached), so this one
                    // spawns unregistered.
                    eprintln!(
                        "crucible-term: daemon: client {conn}: session key already names live session {old}; session {id} is not reattachable"
                    );
                } else {
                    rec.key = Some(k);
                }
            }
            st.sessions.insert(id, rec);
            st.send(
                conn,
                &Msg::Opened {
                    root_pid,
                    reattached: false,
                },
            );
        }
        Err(reason) => {
            eprintln!("crucible-term: daemon: open: {reason}");
            st.send(conn, &Msg::Refused { reason });
        }
    }
}

/// The reattach rule for a *granted* open: if its session key names a detached session of this
/// daemon, bind a new surface to it, move the session to `conn` (pin E1: the old connection
/// gives up ownership, so its EOF can never close the session) and supersede the old client.
/// Returns false when the open should spawn instead (no key, unknown key, attached session).
fn try_reattach<B: Backend>(st: &mut State, b: &mut B, conn: u64, o: &Open) -> bool {
    let Some(id) = o
        .session_key
        .as_deref()
        .and_then(|k| st.find(|r| r.key.as_deref() == Some(k)))
    else {
        return false;
    };
    if !b.is_detached(id) {
        // Attached (or gone): never stolen; `do_open` logs and spawns unregistered.
        return false;
    }
    let root_pid = match b.attach(id, o.grant.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("crucible-term: daemon: client {conn}: reattach session {id}: {e}; spawning");
            return false;
        }
    };
    let rec = st.sessions.entry(id).or_default();
    rec.expiry = None;
    if let Some(old) = rec.conn.replace(conn) {
        st.send(old, &Msg::Superseded);
    }
    eprintln!("crucible-term: daemon: client {conn}: reattached session {id}");
    st.send(
        conn,
        &Msg::Opened {
            root_pid,
            reattached: true,
        },
    );
    true
}

/// Handle one decoded message from `conn`. Returns false to drop the connection.
fn on_msg<B: Backend>(st: &mut State, b: &mut B, cx: &Sched<B>, conn: u64, m: Msg) -> bool {
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
            if c.opening || st.find(|r| r.conn == Some(conn)).is_some() {
                st.send(
                    conn,
                    &Msg::Refused {
                        reason: "already open".into(),
                    },
                );
                return true;
            }
            // Detached: try the display once, synchronously; still lost refuses `no-wayland`
            // so the client falls back to standalone.
            if b.is_lost() && !b.reconnect() {
                st.send(
                    conn,
                    &Msg::Refused {
                        reason: "no-wayland".into(),
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
            if granted && try_reattach(st, b, conn, &o) {
                return true;
            }
            if let Some(d) = take_delay(&mut st.delay, granted) {
                eprintln!(
                    "crucible-term: daemon: client {conn}: open deferred {} ms (CTERM_DEBUG_OPEN_DELAY_MS)",
                    d.as_millis()
                );
                if let Some(c) = st.conns.get_mut(&conn) {
                    c.opening = true;
                }
                arm_deferred_open(cx, conn, d, o);
                return true;
            }
            do_open(st, b, conn, o, granted);
            true
        }
        Msg::Close => {
            if let Some(id) = st.find(|r| r.conn == Some(conn)) {
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

/// Drop `conn`: its window (if still open) closes after [`ORPHAN_GRACE`] (then through the SIGHUP
/// grace) unless the connection is lost first; a detached session outlives its client (a later
/// rebind reattaches it). The grace exists because on a compositor quit the thin clients die
/// with it, and their EOFs can arrive before the daemon sees its own Wayland connection go.
fn drop_conn<B: Backend>(st: &mut State, b: &mut B, conn: u64) {
    st.conns.remove(&conn);
    // Pin E1: only the owning connection may close; a superseded one owns nothing.
    if let Some((&id, rec)) = st.sessions.iter_mut().find(|(_, r)| r.conn == Some(conn)) {
        rec.conn = None;
        if !b.is_detached(id) {
            rec.expiry = Some(Expiry::Orphan(Instant::now() + ORPHAN_GRACE));
        }
    }
}

fn on_readable<B: Backend>(st: &mut State, b: &mut B, cx: &Sched<B>, conn: u64) -> PostAction {
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
                if !on_msg(st, b, cx, conn, m) {
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

/// What `on_msg` needs to schedule work on the loop (the test-only deferred open).
type Sched<B> = (Rc<RefCell<State>>, LoopHandle<'static, B>);

/// TEST-ONLY: run the deferred `open` of `conn` after `d`, on a calloop timer (the loop keeps serving).
fn arm_deferred_open<B: Backend>(cx: &Sched<B>, conn: u64, d: Duration, o: Open) {
    let state = cx.0.clone();
    let mut o = Some(o);
    let r =
        cx.1.insert_source(Timer::from_duration(d), move |_, _, b: &mut B| {
            let mut st = state.borrow_mut();
            let alive = st
                .conns
                .get_mut(&conn)
                .is_some_and(|c| std::mem::take(&mut c.opening));
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
                opening: false,
                token: None,
            },
        );
        drop(st);
        let cx: Sched<B> = (state.clone(), handle.clone());
        let tok = handle.insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, _, b: &mut B| Ok(on_readable(&mut cx.0.borrow_mut(), b, &cx, conn)),
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
        .env_remove(GRANT_VAR)
        .env_remove(SESSION_VAR);
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
    let detached_timeout =
        (settings.detached_timeout > 0).then(|| Duration::from_secs(settings.detached_timeout));
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
        b.identity = env(headless::IDENTITY_VAR).is_some_and(|v| v == "1");
        let ms = |k: &str| {
            headless::Headless::parse_ms(env(k).and_then(|v| v.into_string().ok()).as_deref())
        };
        if let Some(loss) = ms(headless::LOSS_VAR)
            && let Err(e) = b.fake_wayland(loss, ms(headless::RECONNECT_VAR))
        {
            eprintln!("crucible-term: daemon: fake Wayland: {e}");
            return ExitCode::from(1);
        }
        serve(
            event_loop,
            b,
            listener,
            ready,
            linger,
            open_delay,
            detached_timeout,
        )
    } else {
        match Host::new(true) {
            Ok((event_loop, host)) => serve(
                event_loop,
                host,
                listener,
                ready,
                linger,
                open_delay,
                detached_timeout,
            ),
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
    detached_timeout: Option<Duration>,
) -> Result<(), String> {
    let state = Rc::new(RefCell::new(State {
        delay: open_delay,
        detached_timeout,
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
    let mut was_lost = b.is_lost();
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
        let deadline_left = {
            let st = state.borrow();
            st.sessions
                .values()
                .filter_map(|r| r.expiry)
                .map(|(Expiry::Orphan(t) | Expiry::Detached(t))| t)
                .min()
                .map(|d| d.saturating_duration_since(Instant::now()))
        };
        let timeout = [linger_left, deadline_left].into_iter().flatten().min();
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
                let Some(conn) = st.sessions.remove(&id).and_then(|r| r.conn) else {
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
        let lost = b.is_lost();
        detached_clock(
            &mut state.borrow_mut(),
            &mut b,
            was_lost,
            lost,
            Instant::now(),
        );
        was_lost = lost;
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

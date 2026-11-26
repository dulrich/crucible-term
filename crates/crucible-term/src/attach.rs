//! `cterm ls`, `cterm attach` and `cterm kill`: viewer-side clients of the daemon wire, with no
//! Wayland code on their path (they work in an ssh login with no display).
//!
//! All three connect to `$XDG_RUNTIME_DIR/cterm/daemon.sock` and let socket activation start the
//! daemon; they never spawn one and never fall back to standalone. `attach` bridges this
//! terminal to a session: raw mode, the terminal's size in `Attach`, every `Output` written to
//! stdout unmodified, stdin forwarded as `Input` (through the [`Prefix`] state machine only),
//! `Resize` on SIGWINCH. The bridge is one `poll` loop over stdin, stdout (when output is
//! queued), the socket and a signalfd; both outgoing directions queue with offsets, so a slow
//! terminal stalls the bridge but never drops or reorders bytes.
//!
//! Cleanup: the saved termios is restored on every way out (a panic hook covers `panic =
//! "abort"`). On the clean paths (detach, child exit) the daemon's leave sequence already arrived
//! inside `Output`; on the unclean paths (daemon loss, a signal, a panic) the bridge has no mode
//! state, so it writes [`Viewer::leave_all`]. That reset is written only on the unclean paths:
//! after a clean leave the host is on its main screen, where an extra kitty pop would pop the
//! shell's own entry. The `HOST:` forms run ssh and, for `attach`, restore the termios and write
//! the reset after ssh ends, since the far side cannot reach a terminal whose transport is gone.

use crate::cli::Target;
use crate::client::read_msg;
use crate::config;
use crate::daemon::paths;
use crate::wire::{self, Entry, HelloVerdict, Msg};
use crucible_term_core::Viewer;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::termios::{self, OptionalActions, Termios};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a request waits for its reply.
const REPLY_BUDGET: Duration = Duration::from_secs(5);
/// How long `kill` waits: the daemon answers once the child's exit is recorded (SIGHUP grace).
const KILL_BUDGET: Duration = Duration::from_secs(60);
/// How long a sent `Detach` waits for the daemon to close the connection.
const DETACH_BUDGET: Duration = Duration::from_secs(2);
/// How long an incomplete escape sequence on stdin is held before it is forwarded as is.
const PARTIAL_HOLD: Duration = Duration::from_millis(50);
/// A queue this long stops reading the side that fills it (back-pressure, never a drop).
const HIGH_WATER: usize = 1 << 20;

fn code(c: i32) -> ExitCode {
    ExitCode::from(u8::try_from(c).unwrap_or(1))
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("crucible-term: {msg}");
    ExitCode::from(1)
}

/// What a connection that closed with no reply means.
fn closed(build: &str) -> String {
    format!(
        "the daemon (build {build}) closed the connection; if it predates attach (0.6.0), it must be restarted to gain it (or it stopped)"
    )
}

/// A human sentence per refusal reason.
fn refusal(reason: &str, session: &str) -> String {
    match reason {
        "no-session" => format!("no session matches {session:?} (see `cterm ls`)"),
        "ambiguous" => {
            format!("{session:?} matches more than one session; give the id or more of the key")
        }
        "bad-size" => "the daemon refused this terminal's size (1-1000 columns and rows)".into(),
        "already-attached" => "this connection is already attached".into(),
        r => format!("the daemon refused: {r}"),
    }
}

/// A connection past `Hello`.
struct Conn {
    s: UnixStream,
    buf: Vec<u8>,
    build: String,
}

impl Conn {
    fn open() -> Result<Conn, String> {
        let env = |k: &str| std::env::var_os(k);
        let p = paths::socket_paths(&env).map_err(|e| format!("no daemon socket ({e})"))?;
        let mut s = UnixStream::connect(&p.socket).map_err(|e| {
            format!(
                "cannot reach the daemon at {}: {e} (is cterm.socket enabled?)",
                p.socket.display()
            )
        })?;
        let _ = s.set_read_timeout(Some(REPLY_BUDGET));
        let own = env!("FLEET_BUILD_ID");
        let hello = Msg::Hello {
            protocol_version: wire::PROTOCOL_VERSION,
            build_id: own.into(),
        };
        s.write_all(&wire::encode(&hello))
            .map_err(|e| format!("daemon hello: {e}"))?;
        let mut buf = Vec::new();
        match read_msg(&mut s, &mut buf) {
            Ok(Some(m)) => match (&m, wire::judge_hello(&m, own)) {
                (_, HelloVerdict::Fallback) => match m {
                    Msg::Hello {
                        protocol_version,
                        build_id,
                    } => Err(format!(
                        "the daemon (build {build_id}) speaks protocol {protocol_version}, this cterm {}; if it predates attach (0.6.0), it must be restarted to gain it",
                        wire::PROTOCOL_VERSION
                    )),
                    other => Err(format!("the daemon answered {other:?} to hello")),
                },
                (Msg::Hello { build_id, .. }, _) => Ok(Conn {
                    build: build_id.clone(),
                    s,
                    buf,
                }),
                _ => Err("the daemon answered no hello".into()),
            },
            Ok(None) => Err("the daemon closed the connection before its hello".into()),
            Err(e) => Err(format!("daemon hello: {e}")),
        }
    }

    /// Send `m` and wait up to `budget` for one reply; `Err` carries the message to print.
    fn request(&mut self, m: &Msg, budget: Duration) -> Result<Msg, String> {
        self.s
            .write_all(&wire::encode(m))
            .map_err(|e| format!("daemon: {e}"))?;
        let _ = self.s.set_read_timeout(Some(budget));
        match read_msg(&mut self.s, &mut self.buf) {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(closed(&self.build)),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Err("the daemon did not answer in time".into())
            }
            Err(e) => Err(format!("daemon: {e}")),
        }
    }
}

/// Run `ssh ARGS` (looked up on `PATH`) and relay its exit status. With `restore`, the local
/// terminal's termios are saved first and restored afterwards, then [`Viewer::leave_all`] is
/// written, however ssh ended.
fn ssh(args: &[&str], restore: bool) -> ExitCode {
    let stdin = rustix::stdio::stdin();
    let saved = if restore {
        termios::tcgetattr(stdin).ok()
    } else {
        None
    };
    let status = std::process::Command::new("ssh").args(args).status();
    if let Some(t) = &saved {
        let _ = termios::tcsetattr(stdin, OptionalActions::Now, t);
    }
    if restore && termios::isatty(rustix::stdio::stdout()) {
        write_blocking(&Viewer::leave_all());
    }
    match status {
        Ok(s) => {
            use std::os::unix::process::ExitStatusExt;
            code(s.code().or(s.signal().map(|n| 128 + n)).unwrap_or(1))
        }
        Err(e) => fail(&format!("ssh: {e}")),
    }
}

/// Write all of `b` to stdout (a blocking fd), ignoring errors (the terminal may be gone).
fn write_blocking(b: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b);
    let _ = out.flush();
}

/// Replace control characters so a title or path cannot move the listing's cursor.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The `ls` listing: one aligned line per session, no colour, nothing for an empty list.
pub fn format_ls(entries: &[Entry]) -> String {
    let rows: Vec<[String; 8]> = entries
        .iter()
        .map(|e| {
            [
                e.id.to_string(),
                e.key
                    .as_deref()
                    .map_or("-".into(), |k| k.chars().take(8).collect()),
                e.pid.to_string(),
                format!("{}x{}", e.cols, e.rows),
                if e.has_window { "window" } else { "detached" }.into(),
                e.viewers.to_string(),
                printable(&e.title),
                printable(&e.cwd),
            ]
        })
        .collect();
    let mut width = [0usize; 8];
    for r in &rows {
        for (w, f) in width.iter_mut().zip(r) {
            *w = (*w).max(f.chars().count());
        }
    }
    let mut out = String::new();
    for r in &rows {
        let mut line = String::new();
        for (i, f) in r.iter().enumerate() {
            if i == 7 {
                line.push_str(f);
            } else {
                let pad = width[i] - f.chars().count();
                // Numbers right-aligned, words left-aligned.
                if matches!(i, 0 | 2 | 5) {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(f);
                } else {
                    line.push_str(f);
                    line.push_str(&" ".repeat(pad));
                }
                line.push_str("  ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `cterm ls [HOST]`.
pub fn ls(host: Option<String>) -> ExitCode {
    if let Some(h) = host {
        return ssh(&["--", &h, "cterm", "ls"], false);
    }
    let mut c = match Conn::open() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    match c.request(&Msg::List, REPLY_BUDGET) {
        Ok(Msg::Sessions { entries }) => {
            print!("{}", format_ls(&entries));
            ExitCode::SUCCESS
        }
        Ok(Msg::Refused { reason }) => fail(&format!("the daemon refused: {reason}")),
        Ok(m) => fail(&format!("the daemon answered {m:?} to ls")),
        Err(e) => fail(&e),
    }
}

/// `cterm kill [HOST:]SESSION`.
pub fn kill(t: Target) -> ExitCode {
    if let Some(h) = &t.host {
        return ssh(&["--", h, "cterm", "kill", &t.session], false);
    }
    let mut c = match Conn::open() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let m = Msg::Kill {
        session: t.session.clone(),
    };
    match c.request(&m, KILL_BUDGET) {
        Ok(Msg::Exited { .. }) => ExitCode::SUCCESS,
        Ok(Msg::Refused { reason }) => fail(&refusal(&reason, &t.session)),
        Ok(m) => fail(&format!("the daemon answered {m:?} to kill")),
        Err(e) => fail(&e),
    }
}

/// The terminal's `(cols, rows)`, clamped into the wire's bounds (80x24 when unknown).
fn term_size() -> (u32, u32) {
    match termios::tcgetwinsize(rustix::stdio::stdout()) {
        Ok(w) if w.ws_col > 0 && w.ws_row > 0 => (
            u32::from(w.ws_col).min(wire::MAX_GEOMETRY),
            u32::from(w.ws_row).min(wire::MAX_GEOMETRY),
        ),
        _ => (80, 24),
    }
}

/// The termios to restore, shared with the panic hook.
static SAVED: Mutex<Option<Termios>> = Mutex::new(None);

/// Restore the saved termios (once); with `reset`, write [`Viewer::leave_all`] first.
fn restore(reset: bool) {
    let saved = SAVED.lock().map(|mut g| g.take()).unwrap_or(None);
    if let Some(t) = saved {
        if reset {
            write_blocking(&Viewer::leave_all());
        }
        let _ = termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &t);
    }
}

/// Raw mode for the bridge's lifetime; dropping it (or a panic) restores the terminal.
struct RawGuard;

impl RawGuard {
    fn enter() -> Result<RawGuard, String> {
        let stdin = rustix::stdio::stdin();
        let saved = termios::tcgetattr(stdin).map_err(|e| format!("tcgetattr: {e}"))?;
        let mut raw = saved.clone();
        raw.make_raw();
        if let Ok(mut g) = SAVED.lock() {
            *g = Some(saved);
        }
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(true);
            prev(info);
        }));
        termios::tcsetattr(stdin, OptionalActions::Now, &raw).map_err(|e| {
            restore(false);
            format!("tcsetattr: {e}")
        })?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore(true);
    }
}

/// Block SIGWINCH/SIGTERM/SIGHUP/SIGINT and return a signalfd for them.
fn signal_fd() -> Option<OwnedFd> {
    #[allow(unsafe_code)]
    // SAFETY: plain libc calls on a local, zero-initialised sigset; the returned fd is owned by
    // the `OwnedFd` only when signalfd succeeded. This process runs no other thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::sigaddset(&mut set, s);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return None;
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        (fd >= 0).then(|| OwnedFd::from_raw_fd(fd))
    }
}

/// The pending signals, by number.
fn read_signals(fd: &OwnedFd) -> Vec<i32> {
    let mut out = Vec::new();
    let mut b = [0u8; 128];
    // `ssi_signo` is the first u32 of `signalfd_siginfo`.
    while let Ok(128) = rustix::io::read(fd, &mut b) {
        out.push(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as i32);
    }
    out
}

/// `cterm attach [HOST:]SESSION`.
pub fn attach(t: Target) -> ExitCode {
    if let Some(h) = &t.host {
        return ssh(&["-t", "--", h, "cterm", "attach", &t.session], true);
    }
    if !termios::isatty(rustix::stdio::stdin()) || !termios::isatty(rustix::stdio::stdout()) {
        eprintln!("crucible-term: attach needs a terminal on stdin and stdout");
        return ExitCode::from(2);
    }
    let env = |k: &str| std::env::var_os(k);
    let (settings, warnings) = config::load(
        config::config_path(&env).as_deref(),
        &|p| std::fs::read_to_string(p),
        &env,
        &[],
    );
    for w in warnings {
        eprintln!("{w}");
    }
    let mut c = match Conn::open() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let (cols, rows) = term_size();
    let m = Msg::Attach {
        session: t.session.clone(),
        cols,
        rows,
    };
    match c.request(&m, REPLY_BUDGET) {
        Ok(Msg::Attached { .. }) => {}
        Ok(Msg::Refused { reason }) => return fail(&refusal(&reason, &t.session)),
        Ok(m) => return fail(&format!("the daemon answered {m:?} to attach")),
        Err(e) => return fail(&e),
    }
    let sig = signal_fd();
    let guard = match RawGuard::enter() {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };
    let end = Bridge::new(c, Prefix::new(settings.attach_prefix)).run(sig.as_ref());
    match end {
        End::Detached => {
            std::mem::forget(guard);
            restore(false);
            ExitCode::SUCCESS
        }
        End::Exited(c) => {
            std::mem::forget(guard);
            restore(false);
            code(c)
        }
        End::Refused(r) => {
            drop(guard);
            fail(&refusal(&r, &t.session))
        }
        End::Lost(why) => {
            drop(guard);
            fail(&why)
        }
        End::Signal(n) => {
            drop(guard);
            code(128 + n)
        }
    }
}

/// How the bridge ended.
#[derive(Debug)]
enum End {
    /// The daemon closed the connection after our `Detach` (its leave sequence was written).
    Detached,
    /// The session's child exited with this code (the leave sequence was written).
    Exited(i32),
    /// The daemon refused something mid-stream.
    Refused(String),
    /// The daemon went away (or broke the protocol): the message to print.
    Lost(String),
    /// SIGTERM, SIGHUP or SIGINT.
    Signal(i32),
}

/// Bytes waiting for a non-blocking or slow writer, with the written offset.
#[derive(Default)]
struct Queue {
    buf: Vec<u8>,
    at: usize,
}

impl Queue {
    fn pending(&self) -> &[u8] {
        &self.buf[self.at..]
    }

    fn len(&self) -> usize {
        self.buf.len() - self.at
    }

    fn push(&mut self, b: &[u8]) {
        if self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
        } else if self.at > 1 << 16 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        self.buf.extend_from_slice(b);
    }

    fn written(&mut self, n: usize) {
        self.at += n;
    }
}

struct Bridge {
    c: Conn,
    prefix: Prefix,
    out: Queue,
    to_sock: Queue,
    detach_by: Option<Instant>,
    stdin_open: bool,
}

/// One poll pass's readiness.
#[derive(Default)]
struct Ready {
    stdin: bool,
    sock: bool,
    stdout: bool,
    sig: bool,
}

impl Bridge {
    fn new(c: Conn, prefix: Prefix) -> Bridge {
        Bridge {
            c,
            prefix,
            out: Queue::default(),
            to_sock: Queue::default(),
            detach_by: None,
            stdin_open: true,
        }
    }

    fn send(&mut self, m: &Msg) {
        self.to_sock.push(&wire::encode(m));
    }

    fn detach(&mut self) {
        if self.detach_by.is_none() {
            self.send(&Msg::Detach);
            self.detach_by = Some(Instant::now() + DETACH_BUDGET);
        }
    }

    /// Write every queued output byte (the clean exits), blocking.
    fn drain_out(&mut self) {
        write_blocking(self.out.pending());
        self.out = Queue::default();
    }

    /// Handle the complete frames in the read buffer.
    fn frames(&mut self) -> Option<End> {
        loop {
            match wire::decode(&self.c.buf) {
                Ok(Some((m, n))) => {
                    self.c.buf.drain(..n);
                    match m {
                        Msg::Output { bytes } => self.out.push(&bytes),
                        Msg::Exited { code } => {
                            self.drain_out();
                            return Some(End::Exited(code));
                        }
                        Msg::Refused { reason } => {
                            self.drain_out();
                            return Some(End::Refused(reason));
                        }
                        _ => {}
                    }
                }
                Ok(None) => return None,
                Err(e) => return Some(End::Lost(format!("daemon sent a malformed frame: {e}"))),
            }
        }
    }

    fn poll(&self, sig: Option<&OwnedFd>) -> Result<Ready, End> {
        let stdin = rustix::stdio::stdin();
        let stdout = rustix::stdio::stdout();
        let read_stdin =
            self.stdin_open && self.detach_by.is_none() && self.to_sock.len() < HIGH_WATER;
        let mut sock_flags = PollFlags::empty();
        if self.out.len() < HIGH_WATER {
            sock_flags |= PollFlags::IN;
        }
        if self.to_sock.len() > 0 {
            sock_flags |= PollFlags::OUT;
        }
        let mut fds = vec![PollFd::new(&self.c.s, sock_flags)];
        let i_in = read_stdin.then(|| {
            fds.push(PollFd::new(&stdin, PollFlags::IN));
            fds.len() - 1
        });
        let i_out = (self.out.len() > 0).then(|| {
            fds.push(PollFd::new(&stdout, PollFlags::OUT));
            fds.len() - 1
        });
        let i_sig = sig.map(|s| {
            fds.push(PollFd::new(s, PollFlags::IN));
            fds.len() - 1
        });
        let mut wait: Option<Duration> = None;
        if self.prefix.holding() {
            wait = Some(PARTIAL_HOLD);
        }
        if let Some(by) = self.detach_by {
            let left = by.saturating_duration_since(Instant::now());
            wait = Some(wait.map_or(left, |w| w.min(left)));
        }
        let ts = wait.map(|d| rustix::event::Timespec {
            tv_sec: d.as_secs() as i64,
            tv_nsec: i64::from(d.subsec_nanos()),
        });
        match poll(&mut fds, ts.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(End::Lost(format!("poll: {e}"))),
        }
        let hit = |i: Option<usize>| i.is_some_and(|i| !fds[i].revents().is_empty());
        Ok(Ready {
            sock: !fds[0].revents().is_empty(),
            stdin: hit(i_in),
            stdout: hit(i_out),
            sig: hit(i_sig),
        })
    }

    fn run(mut self, sig: Option<&OwnedFd>) -> End {
        let _ = self.c.s.set_nonblocking(true);
        loop {
            if let Some(end) = self.frames() {
                return end;
            }
            let r = match self.poll(sig) {
                Ok(r) => r,
                Err(end) => return end,
            };
            if !r.stdin && self.prefix.holding() && self.prefix.held_for() >= PARTIAL_HOLD {
                let mut fwd = Vec::new();
                self.prefix.flush(&mut fwd);
                self.input(&fwd);
            }
            if self.detach_by.is_some_and(|by| Instant::now() >= by) {
                return End::Lost("the daemon did not finish the detach".into());
            }
            if r.sig
                && let Some(s) = sig
            {
                for n in read_signals(s) {
                    if n == libc::SIGWINCH {
                        let (cols, rows) = term_size();
                        self.send(&Msg::Resize { cols, rows });
                    } else {
                        return End::Signal(n);
                    }
                }
            }
            if r.stdout {
                let p = self.out.pending();
                let n = p.len().min(wire::OUTPUT_CHUNK);
                match rustix::io::write(rustix::stdio::stdout(), &p[..n]) {
                    Ok(n) => self.out.written(n),
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                    Err(e) => return End::Lost(format!("stdout: {e}")),
                }
            }
            if r.stdin {
                let mut b = [0u8; 4096];
                match rustix::io::read(rustix::stdio::stdin(), &mut b) {
                    Ok(0) | Err(rustix::io::Errno::IO) => {
                        self.stdin_open = false;
                        self.detach();
                    }
                    Ok(n) => {
                        let mut fwd = Vec::new();
                        let detach = self.prefix.feed(&b[..n], &mut fwd);
                        self.input(&fwd);
                        if detach {
                            self.detach();
                        }
                    }
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                    Err(e) => return End::Lost(format!("stdin: {e}")),
                }
            }
            if r.sock {
                if let Some(end) = self.sock_io() {
                    return end;
                }
            }
        }
    }

    fn input(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.send(&Msg::Input {
                bytes: bytes.to_vec(),
            });
        }
    }

    /// Read what the socket has and write what it takes.
    fn sock_io(&mut self) -> Option<End> {
        if self.out.len() < HIGH_WATER {
            let mut chunk = [0u8; 65536];
            loop {
                match self.c.s.read(&mut chunk) {
                    Ok(0) => {
                        if let Some(end) = self.frames() {
                            return Some(end);
                        }
                        if self.detach_by.is_some() {
                            self.drain_out();
                            return Some(End::Detached);
                        }
                        return Some(End::Lost("daemon connection lost".into()));
                    }
                    Ok(n) => {
                        self.c.buf.extend_from_slice(&chunk[..n]);
                        if n < chunk.len() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e)
                        if self.detach_by.is_some() && e.kind() == ErrorKind::ConnectionReset =>
                    {
                        self.drain_out();
                        return Some(End::Detached);
                    }
                    Err(e) => return Some(End::Lost(format!("daemon connection lost: {e}"))),
                }
            }
        }
        while self.to_sock.len() > 0 {
            match self.c.s.write(self.to_sock.pending()) {
                Ok(n) => self.to_sock.written(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Some(End::Lost(format!("daemon connection lost: {e}"))),
            }
        }
        None
    }
}

/// The prefix key and the `d` after it, recognised in the stdin byte stream in both the legacy
/// form (a C0 byte, a plain `d`) and the kitty keyboard form (`CSI code ; mods[:event] u`, with
/// the alternate-key and text sub-fields), across read boundaries. Everything else passes
/// through unchanged and in order. Pure; the bridge feeds it reads and flushes a held escape
/// prefix after [`PARTIAL_HOLD`].
pub struct Prefix {
    /// The legacy C0 byte of the prefix.
    byte: u8,
    /// The prefix's kitty key code (its unshifted character).
    code: u32,
    /// The prefix was pressed: its bytes (and any held release events) wait here.
    armed: Option<Vec<u8>>,
    /// An escape sequence still being read.
    partial: Vec<u8>,
    /// When `partial` started.
    since: Option<Instant>,
}

/// A key token's meaning to the state machine.
#[derive(Debug, PartialEq, Eq)]
enum Class {
    Prefix,
    D,
    /// A kitty release event or a lone modifier key: does not end an armed prefix.
    Neutral,
    Other,
}

const ESC: u8 = 0x1b;

impl Prefix {
    /// The state machine for `Ctrl-<c>` (`c` as config stores it: lowercase, or a symbol).
    pub fn new(c: char) -> Prefix {
        Prefix {
            byte: (c.to_ascii_uppercase() as u8) & 0x1f,
            code: c as u32,
            armed: None,
            partial: Vec::new(),
            since: None,
        }
    }

    /// An incomplete escape sequence is held.
    pub fn holding(&self) -> bool {
        !self.partial.is_empty()
    }

    fn held_for(&self) -> Duration {
        self.since.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Forward a held incomplete escape sequence as it is (its completion did not arrive).
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        let p = std::mem::take(&mut self.partial);
        self.since = None;
        if !p.is_empty() {
            self.token(p, out);
        }
    }

    /// Feed stdin bytes; the bytes for the session are appended to `out`. True: detach now (the
    /// rest of `input` is dropped).
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> bool {
        for &b in input {
            if self.byte_in(b, out) {
                return true;
            }
        }
        false
    }

    fn byte_in(&mut self, b: u8, out: &mut Vec<u8>) -> bool {
        match self.partial.len() {
            0 if b == ESC => {
                self.partial.push(b);
                self.since = Some(Instant::now());
                false
            }
            0 => self.token(vec![b], out),
            1 if b == b'[' => {
                self.partial.push(b);
                false
            }
            1 if b == ESC => {
                // A lone ESC, then a new escape.
                let done = self.take_token(out);
                self.partial.push(b);
                self.since = Some(Instant::now());
                done
            }
            1 => {
                // ESC + one byte (an Alt chord): one key.
                self.partial.push(b);
                self.take_token(out)
            }
            _ => match b {
                0x20..=0x3f if self.partial.len() < 64 => {
                    self.partial.push(b);
                    false
                }
                0x40..=0x7e => {
                    self.partial.push(b);
                    self.take_token(out)
                }
                _ => {
                    // Not a CSI after all: what was held is one token, `b` starts afresh.
                    self.take_token(out) || self.byte_in(b, out)
                }
            },
        }
    }

    fn take_token(&mut self, out: &mut Vec<u8>) -> bool {
        let p = std::mem::take(&mut self.partial);
        self.since = None;
        self.token(p, out)
    }

    fn class(&self, tok: &[u8]) -> Class {
        if tok == [self.byte] {
            return Class::Prefix;
        }
        if tok == b"d" {
            return Class::D;
        }
        let Some((code, mods, event)) = kitty(tok) else {
            return Class::Other;
        };
        // Caps Lock (64) and Num Lock (128) do not count as modifiers here.
        let m = mods.saturating_sub(1) & !(64 | 128);
        if event == 3 || (57441..=57452).contains(&code) {
            Class::Neutral
        } else if code == self.code && m == 4 {
            Class::Prefix
        } else if code == u32::from(b'd') && m == 0 {
            Class::D
        } else {
            Class::Other
        }
    }

    fn token(&mut self, tok: Vec<u8>, out: &mut Vec<u8>) -> bool {
        let class = self.class(&tok);
        match (self.armed.take(), class) {
            (None, Class::Prefix) => self.armed = Some(tok),
            (None, _) => out.extend_from_slice(&tok),
            (Some(mut held), Class::Neutral) => {
                held.extend_from_slice(&tok);
                self.armed = Some(held);
            }
            (Some(_), Class::D) => return true,
            // The prefix twice: one prefix reaches the session.
            (Some(held), Class::Prefix) => out.extend_from_slice(&held),
            (Some(held), Class::Other) => {
                out.extend_from_slice(&held);
                out.extend_from_slice(&tok);
            }
        }
        false
    }
}

/// A kitty key event `CSI code[:alt...] [; mods[:event] [; text]] u` as `(code, mods, event)`.
fn kitty(tok: &[u8]) -> Option<(u32, u32, u32)> {
    let body = tok.strip_prefix(b"\x1b[")?.strip_suffix(b"u")?;
    let s = std::str::from_utf8(body).ok()?;
    let mut fields = s.split(';');
    let code = fields.next()?.split(':').next()?.parse().ok()?;
    let (mods, event) = match fields.next() {
        None => (1, 1),
        Some(f) => {
            let mut p = f.split(':');
            let num = |v: Option<&str>| match v {
                None | Some("") => Some(1),
                Some(n) => n.parse().ok(),
            };
            (num(p.next())?, num(p.next())?)
        }
    };
    Some((code, mods, event))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `chunks` in order; the forwarded bytes and whether a detach fired.
    fn run(c: char, chunks: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut p = Prefix::new(c);
        let mut out = Vec::new();
        for ch in chunks {
            if p.feed(ch, &mut out) {
                return (out, true);
            }
        }
        p.flush(&mut out);
        (out, false)
    }

    #[test]
    fn legacy_prefix_forms() {
        assert_eq!(run('\\', &[b"ab\x1cd"]), (b"ab".to_vec(), true));
        assert_eq!(run('\\', &[b"\x1c", b"d"]), (b"".to_vec(), true));
        assert_eq!(run('\\', &[b"\x1c\x1cx"]), (b"\x1cx".to_vec(), false));
        assert_eq!(run('\\', &[b"\x1cx"]), (b"\x1cx".to_vec(), false));
        assert_eq!(run('\\', &[b"\x1c\x1b[A"]), (b"\x1c\x1b[A".to_vec(), false));
        assert_eq!(run('a', &[b"\x01d"]), (b"".to_vec(), true));
        assert_eq!(run('a', &[b"\x1cd"]), (b"\x1cd".to_vec(), false));
        // Plain escapes and an Alt chord pass through whole.
        assert_eq!(
            run('\\', &[b"\x1b[1;5A\x1bd\x1b"]),
            (b"\x1b[1;5A\x1bd\x1b".to_vec(), false)
        );
    }

    #[test]
    fn kitty_prefix_forms_across_reads() {
        assert_eq!(run('\\', &[b"\x1b[92;5u", b"d"]), (b"".to_vec(), true));
        assert_eq!(
            run('\\', &[b"\x1b[9", b"2;5", b"u\x1b[", b"100u"]),
            (b"".to_vec(), true)
        );
        // Event types and alternate keys; the release between does not disarm.
        assert_eq!(
            run('\\', &[b"\x1b[92:124;5:1u\x1b[92;5:3u\x1b[100;1:1u"]),
            (b"".to_vec(), true)
        );
        // Caps Lock on still counts; Ctrl+Shift does not.
        assert_eq!(run('\\', &[b"\x1b[92;69ud"]), (b"".to_vec(), true));
        assert_eq!(
            run('\\', &[b"\x1b[92;6ud"]),
            (b"\x1b[92;6ud".to_vec(), false)
        );
        // Twice: one prefix (the first press with its release).
        assert_eq!(
            run('\\', &[b"\x1b[92;5u\x1b[92;5:3u\x1b[92;5u"]),
            (b"\x1b[92;5u\x1b[92;5:3u".to_vec(), false)
        );
        // Anything else: both.
        assert_eq!(
            run('\\', &[b"\x1b[92;5u\x1b[101u"]),
            (b"\x1b[92;5u\x1b[101u".to_vec(), false)
        );
    }

    #[test]
    fn ls_lines_are_aligned() {
        let e = |id, key: Option<&str>, w, title: &str| Entry {
            id,
            key: key.map(Into::into),
            pid: 4242,
            title: title.into(),
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
            has_window: w,
            viewers: 1,
        };
        let s = format_ls(&[
            e(3, Some("0123456789abcdef0123456789abcdef"), true, "vim\x07"),
            e(12, None, false, ""),
        ]);
        assert_eq!(
            s,
            " 3  01234567  4242  80x24  window    1  vim?  /tmp\n12  -         4242  80x24  detached  1        /tmp\n"
        );
        assert_eq!(format_ls(&[]), "");
    }
}

//! The thin client: mode selection, grant stripping, and the `cterm` side of the daemon wire.
//!
//! [`run`] reaches the per-display daemon (starting it on demand under `flock` on the sibling
//! `.lock`, with a readiness pipe and a 2 s budget), sends `hello` and `open`, then waits for
//! `exited` and returns its code. SIGTERM/SIGINT send `close` (the daemon SIGHUPs the child
//! with its grace) and the client exits with the relayed code or 128 + signo; SIGHUP only hangs up
//! (exit 129): the daemon closes the session after its orphan grace unless its own Wayland
//! connection is lost first, which keeps the session for reattach across a logout. Any failure
//! before `opened` is a [`Outcome::Fallback`]: the caller runs standalone with one warning.

use crate::config::RunMode;
use crate::daemon::{self, paths};
use crate::wire::{self, HelloVerdict, Msg, Open};
use rustix::event::{PollFd, PollFlags, poll};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// The CDE window grant variable; it never reaches a child's environment.
pub const GRANT_VAR: &str = "CRUCIBLE_WINDOW_GRANT";

/// How long the client gives the daemon to start, and to answer `hello`/`open`.
const START_BUDGET: Duration = Duration::from_secs(2);
/// How long a signalled client waits for the `exited` that follows its `close`.
const CLOSE_WAIT: Duration = Duration::from_millis(1500);

/// The CDE session key variable; it never reaches a child's environment.
pub const SESSION_VAR: &str = "CRUCIBLE_TERM_SESSION";

/// `env` without `CRUCIBLE_WINDOW_GRANT` or `CRUCIBLE_TERM_SESSION`.
pub fn strip_grant(env: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
    env.into_iter()
        .filter(|(k, _)| k != GRANT_VAR && k != SESSION_VAR)
        .collect()
}

/// A session key is exactly 32 lowercase hex chars.
pub fn valid_key(k: &str) -> bool {
    k.len() == 32
        && k.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The `open.session_key` for a raw `CRUCIBLE_TERM_SESSION` value: invalid is `None`, logged once.
pub fn session_key(raw: Option<OsString>) -> Option<String> {
    let raw = raw?;
    match raw.to_str() {
        Some(k) if valid_key(k) => Some(k.to_owned()),
        _ => {
            eprintln!("crucible-term: ignoring malformed {SESSION_VAR} (want 32 lowercase hex)");
            None
        }
    }
}

/// Where this window runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// In this process.
    Standalone,
    /// Through the daemon.
    Daemon,
}

/// The mode decision: `--standalone` wins, `auto` uses the daemon only with a grant.
pub fn route(mode: RunMode, grant: bool, force_standalone: bool) -> Route {
    match mode {
        _ if force_standalone => Route::Standalone,
        RunMode::Daemon => Route::Daemon,
        RunMode::Auto if grant => Route::Daemon,
        RunMode::Auto | RunMode::Standalone => Route::Standalone,
    }
}

/// How a client run ended.
#[derive(Debug)]
pub enum Outcome {
    /// The window ran in the daemon; exit with this code.
    Exit(i32),
    /// The daemon refused the open for another reason than identity; exit 1 with this.
    Failed(String),
    /// Run standalone after this one-line warning.
    Fallback(String),
}

/// Read frames from a blocking `stream` until one decodes; `Ok(None)` on EOF.
fn read_msg(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<Option<Msg>> {
    loop {
        match wire::decode(buf) {
            Ok(Some((m, n))) => {
                buf.drain(..n);
                return Ok(Some(m));
            }
            Ok(None) => {}
            Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Spawn `cterm daemon` detached and wait (within `deadline`) for its readiness byte.
fn start_daemon(log: Option<std::path::PathBuf>, deadline: Instant) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Both ends without CLOEXEC from pipe(2); the read end gets it back at once.
    let (r, w) = rustix::pipe::pipe().map_err(|e| format!("pipe: {e}"))?;
    rustix::io::fcntl_setfd(&r, rustix::io::FdFlags::CLOEXEC).map_err(|e| e.to_string())?;
    let (out, err) = match log {
        Some(path) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let g = f.try_clone().map_err(|e| e.to_string())?;
            (std::process::Stdio::from(f), std::process::Stdio::from(g))
        }
        None => (std::process::Stdio::null(), std::process::Stdio::null()),
    };
    let mut stage1 = std::process::Command::new(exe)
        .arg("daemon")
        .env_remove(GRANT_VAR)
        .env_remove(SESSION_VAR)
        .env(daemon::DETACH_VAR, "1")
        .env(daemon::READY_FD_VAR, w.as_raw_fd().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    drop(w);
    let mut ready = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let ts = rustix::event::Timespec {
            tv_sec: left.as_secs() as i64,
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        let mut fds = [PollFd::new(&r, PollFlags::IN)];
        match poll(&mut fds, Some(&ts)) {
            Ok(0) => break,
            Ok(_) => {
                let mut b = [0u8; 1];
                ready = matches!(rustix::io::read(&r, &mut b), Ok(1));
                break;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(format!("poll: {e}")),
        }
    }
    // The first stage exits as soon as it has spawned the daemon; reap it (no zombie).
    loop {
        match stage1.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) if Instant::now() < deadline + Duration::from_millis(200) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = stage1.kill();
                let _ = stage1.wait();
                break;
            }
        }
    }
    if ready {
        Ok(())
    } else {
        Err("the daemon did not become ready within 2 s".into())
    }
}

/// Connect to the daemon, starting it under the lock if nothing listens.
fn connect(p: &paths::Paths, log: Option<std::path::PathBuf>) -> Result<UnixStream, String> {
    if let Ok(s) = UnixStream::connect(&p.socket) {
        return Ok(s);
    }
    let deadline = Instant::now() + START_BUDGET;
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&p.dir)
            .map_err(|e| format!("{}: {e}", p.dir.display()))?;
    }
    let lock = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&p.lock)
            .map_err(|e| format!("{}: {e}", p.lock.display()))?
    };
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| format!("flock {}: {e}", p.lock.display()))?;
    if let Ok(s) = UnixStream::connect(&p.socket) {
        return Ok(s);
    }
    start_daemon(log, deadline)?;
    let s = UnixStream::connect(&p.socket).map_err(|e| format!("connect: {e}"))?;
    drop(lock);
    Ok(s)
}

/// Block SIGTERM/SIGINT/SIGHUP and return a signalfd for them.
fn signal_fd() -> Option<OwnedFd> {
    #[allow(unsafe_code)]
    // SAFETY: plain libc calls on a local, zero-initialised sigset; the returned fd is owned
    // by the `OwnedFd` only when signalfd succeeded. No threads exist whose mask matters.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return None;
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC);
        (fd >= 0).then(|| OwnedFd::from_raw_fd(fd))
    }
}

/// Read one signal number from the signalfd.
fn read_signal(fd: &OwnedFd) -> i32 {
    let mut b = [0u8; 128];
    match rustix::io::read(fd, &mut b) {
        // `ssi_signo` is the first u32 of `signalfd_siginfo`.
        Ok(128) => u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as i32,
        _ => libc::SIGTERM,
    }
}

/// Run one window through the daemon.
pub fn run(open: Open) -> Outcome {
    let env = |k: &str| std::env::var_os(k);
    let p = match paths::socket_paths(&env) {
        Ok(p) => p,
        Err(e) => return Outcome::Fallback(format!("no daemon socket ({e})")),
    };
    let mut stream = match connect(&p, paths::log_path(&env)) {
        Ok(s) => s,
        Err(e) => return Outcome::Fallback(format!("daemon unavailable ({e})")),
    };
    let _ = stream.set_read_timeout(Some(START_BUDGET));
    let mut buf = Vec::new();
    let own = env!("FLEET_BUILD_ID");
    let hello = Msg::Hello {
        protocol_version: wire::PROTOCOL_VERSION,
        build_id: own.into(),
    };
    if let Err(e) = stream.write_all(&wire::encode(&hello)) {
        return Outcome::Fallback(format!("daemon hello: {e}"));
    }
    match read_msg(&mut stream, &mut buf) {
        Ok(Some(m)) => {
            if wire::judge_hello(&m, own) == HelloVerdict::Fallback {
                return Outcome::Fallback(format!("daemon protocol mismatch ({m:?})"));
            }
        }
        Ok(None) => return Outcome::Fallback("daemon closed the connection".into()),
        Err(e) => return Outcome::Fallback(format!("daemon hello: {e}")),
    }
    if let Err(e) = stream.write_all(&wire::encode(&Msg::Open(open))) {
        return Outcome::Fallback(format!("daemon open: {e}"));
    }
    match read_msg(&mut stream, &mut buf) {
        Ok(Some(Msg::Opened { .. })) => {}
        Ok(Some(Msg::Refused { reason })) if reason == "no-identity" => {
            return Outcome::Fallback("the compositor grants no window identity".into());
        }
        Ok(Some(Msg::Refused { reason })) if reason == "no-wayland" => {
            return Outcome::Fallback("the daemon has no Wayland connection".into());
        }
        Ok(Some(Msg::Refused { reason })) => return Outcome::Failed(reason),
        Ok(Some(m)) => return Outcome::Fallback(format!("daemon answered {m:?} to open")),
        Ok(None) => return Outcome::Fallback("daemon closed the connection".into()),
        Err(e) => return Outcome::Fallback(format!("daemon open: {e}")),
    }
    let _ = stream.set_read_timeout(None);
    wait(stream, buf)
}

const LOST: &str = "crucible-term: daemon connection lost";

/// The window is open: wait for `exited`, relaying signals as `close`.
fn wait(mut stream: UnixStream, mut buf: Vec<u8>) -> Outcome {
    let sig = signal_fd();
    loop {
        // A frame may already be buffered.
        if let Ok(Some((m, n))) = wire::decode(&buf) {
            buf.drain(..n);
            match m {
                Msg::Exited { code } => return Outcome::Exit(code),
                // Another window reattached this session: leave quietly.
                Msg::Superseded => return Outcome::Exit(0),
                _ => {}
            }
            continue;
        }
        let (sock_ready, sig_ready) = {
            let mut fds = vec![PollFd::new(&stream, PollFlags::IN)];
            if let Some(s) = &sig {
                fds.push(PollFd::new(s, PollFlags::IN));
            }
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    eprintln!("{LOST}");
                    return Outcome::Exit(1);
                }
            }
            (
                !fds[0].revents().is_empty(),
                fds.get(1).is_some_and(|f| !f.revents().is_empty()),
            )
        };
        if sig_ready && let Some(s) = &sig {
            let signo = read_signal(s);
            if signo == libc::SIGHUP {
                // The session or tty is going away (a logout, a compositor quit), not a request to
                // close the window: hang up without `close`, so the daemon's orphan grace can
                // detach the session when its Wayland connection goes too.
                return Outcome::Exit(128 + signo);
            }
            let _ = stream.write_all(&wire::encode(&Msg::Close));
            let _ = stream.set_read_timeout(Some(CLOSE_WAIT));
            let start = Instant::now();
            while start.elapsed() < CLOSE_WAIT {
                match read_msg(&mut stream, &mut buf) {
                    Ok(Some(Msg::Exited { code })) => return Outcome::Exit(code),
                    Ok(Some(Msg::Superseded)) => return Outcome::Exit(0),
                    Ok(Some(_)) => {}
                    _ => break,
                }
            }
            return Outcome::Exit(128 + signo);
        }
        if sock_ready {
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => {
                    eprintln!("{LOST}");
                    return Outcome::Exit(1);
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            if wire::decode(&buf).is_err() {
                eprintln!("{LOST}");
                return Outcome::Exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_is_stripped() {
        let env = vec![
            ("A".into(), "1".into()),
            (GRANT_VAR.into(), "secret".into()),
            ("CRUCIBLE_WINDOW_GRANTX".into(), "keep".into()),
        ];
        let out = strip_grant(env);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|(k, _)| k != GRANT_VAR));
    }

    #[test]
    fn the_session_key_is_stripped_and_validated() {
        let env = vec![("A".into(), "1".into()), (SESSION_VAR.into(), "k".into())];
        assert_eq!(strip_grant(env), vec![("A".into(), "1".into())]);
        let good = "0123456789abcdef0123456789abcdef";
        assert_eq!(session_key(Some(good.into())), Some(good.into()));
        for bad in [
            "",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef",
            "g123456789abcdef0123456789abcdef",
        ] {
            assert_eq!(session_key(Some(bad.into())), None, "{bad}");
        }
        assert_eq!(session_key(None), None);
    }

    #[test]
    fn mode_decision() {
        use RunMode::*;
        for (mode, grant, force, want) in [
            (Standalone, false, false, Route::Standalone),
            (Standalone, true, false, Route::Standalone),
            (Daemon, false, false, Route::Daemon),
            (Daemon, true, false, Route::Daemon),
            (Daemon, true, true, Route::Standalone),
            (Auto, false, false, Route::Standalone),
            (Auto, true, false, Route::Daemon),
            (Auto, true, true, Route::Standalone),
        ] {
            assert_eq!(route(mode, grant, force), want, "{mode:?} {grant} {force}");
        }
    }
}

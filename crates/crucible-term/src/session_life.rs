//! The session lifecycle every backend shares, generic over the calloop data `D`: the pidfd
//! that reports the child's exit, and the close grace (SIGHUP, poll `try_wait` against a
//! deadline, then SIGKILL and `wait`, recorded as `128 + 1`).

use crucible_term_core::pty::{Pty, exit_code};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use std::time::{Duration, Instant};

/// How long a closed window's child gets to exit on SIGHUP before it counts as killed.
pub(crate) const CLOSE_GRACE: Duration = Duration::from_millis(500);
/// Poll period of the close-grace timer (it exists only while a close is pending).
const CLOSE_POLL: Duration = Duration::from_millis(10);

/// One step of the close grace: the child's `try_wait` result at `now` against `deadline`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Grace {
    /// Not exited yet; poll again.
    Wait,
    /// The exit code to record (`128 + 1` once the deadline passed, as if killed by SIGHUP).
    Done(i32),
}

pub(crate) fn grace_step(
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

fn signal(pid: u32, sig: rustix::process::Signal) {
    if let Some(p) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process(p, sig);
    }
}

/// Watch `pid` through a pidfd and call `on_exit` once it exits, so a background grandchild
/// holding the slave cannot keep a session open after its shell is gone. Without pidfd
/// (`Ok(None)`, logged) the caller's PTY EOF path still ends the session.
pub(crate) fn watch_exit<D: 'static>(
    handle: &LoopHandle<'static, D>,
    pid: u32,
    mut on_exit: impl FnMut(&mut D) + 'static,
) -> Result<Option<RegistrationToken>, String> {
    match rustix::process::Pid::from_raw(pid as i32)
        .map(|p| rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()))
    {
        Some(Ok(fd)) => handle
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                move |_, _, d| {
                    on_exit(d);
                    Ok(PostAction::Remove)
                },
            )
            .map(Some)
            .map_err(|e| e.to_string()),
        Some(Err(e)) => {
            eprintln!("crucible-term: pidfd_open: {e}; relying on PTY EOF");
            Ok(None)
        }
        None => Ok(None),
    }
}

/// SIGHUP `pty`'s child, then poll `try_wait` on a loop timer until it exits or the grace
/// deadline passes (then SIGKILL and reap at once, so a long-lived daemon keeps no zombie).
/// `done` gets the code once. Returns false if the timer could not be inserted: the caller then
/// records `128 + 1` itself. Never blocks the loop the other sessions share.
pub(crate) fn begin_grace<D: 'static>(
    handle: &LoopHandle<'static, D>,
    mut pty: Pty,
    mut done: impl FnMut(&mut D, i32) + 'static,
) -> bool {
    signal(pty.pid(), rustix::process::Signal::HUP);
    let deadline = Instant::now() + CLOSE_GRACE;
    handle
        .insert_source(Timer::from_duration(Duration::ZERO), move |_, _, d| {
            let status = pty.try_wait();
            let exited = matches!(status, Ok(Some(_)));
            match grace_step(status, Instant::now(), deadline) {
                Grace::Wait => TimeoutAction::ToDuration(CLOSE_POLL),
                Grace::Done(code) => {
                    if !exited {
                        // Past the deadline: SIGKILL and reap at once. The code stays 128 + 1.
                        signal(pty.pid(), rustix::process::Signal::KILL);
                        let _ = pty.wait();
                    }
                    done(d, code);
                    TimeoutAction::Drop
                }
            }
        })
        .is_ok()
}

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

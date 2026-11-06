//! TEST-ONLY backend: the Wayland-less daemon (`CTERM_DEBUG_DAEMON_HEADLESS=1`).
//!
//! It exists so integration tests can drive the real daemon code paths (listener, peer check,
//! lock race, exit relay, close → SIGHUP, grant stripping) without a compositor. `open` spawns
//! the PTY and child with no surface and drains the PTY master so the child never blocks on
//! output. It is not a user feature and is not documented as one.

use super::Backend;
use crucible_term_core::pty::{Pty, SpawnSpec};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::{
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::time::{Duration, Instant};

/// SIGHUP grace before SIGKILL, as the Wayland host's close grace.
const GRACE: Duration = Duration::from_millis(500);

struct Kid {
    pty: Pty,
    tokens: Vec<RegistrationToken>,
    /// Set once closed: the SIGHUP grace deadline.
    deadline: Option<Instant>,
}

/// The headless backend; the calloop data in headless mode.
pub struct Headless {
    handle: LoopHandle<'static, Headless>,
    next: u64,
    kids: BTreeMap<u64, Kid>,
    exits: Vec<(u64, i32)>,
}

impl Headless {
    /// A backend inserting its sources through `handle`.
    pub fn new(handle: LoopHandle<'static, Headless>) -> Headless {
        Headless {
            handle,
            next: 1,
            kids: BTreeMap::new(),
            exits: Vec::new(),
        }
    }
}

fn exit_code(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}

fn signal(pid: u32, sig: rustix::process::Signal) {
    if let Some(p) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process(p, sig);
    }
}

impl Backend for Headless {
    fn open(
        &mut self,
        cfg: &crate::window::Config,
        argv: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        _grant: Option<String>,
    ) -> Result<(u64, u32), String> {
        let pty = Pty::spawn(&SpawnSpec {
            argv,
            cwd: cfg.cwd.clone(),
            env,
            size: (cfg.cols, cfg.rows),
        })
        .map_err(|e| format!("pty: {e}"))?;
        let id = self.next;
        self.next += 1;
        let pid = pty.pid();
        let mut master = pty.master().try_clone().map_err(|e| e.to_string())?;
        rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| e.to_string())?;
        let mut tokens = Vec::new();
        let drain = Generic::new(
            rustix::io::dup(&master).map_err(|e| e.to_string())?,
            Interest::READ,
            Mode::Level,
        );
        tokens.push(
            self.handle
                .insert_source(drain, move |_, _, _| {
                    let mut buf = [0u8; 4096];
                    loop {
                        match master.read(&mut buf) {
                            Ok(0) => return Ok(PostAction::Remove),
                            Ok(_) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                return Ok(PostAction::Continue);
                            }
                            Err(_) => return Ok(PostAction::Remove),
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        // The pidfd only wakes the loop; `reap` does the `try_wait`.
        if let Some(Ok(fd)) = rustix::process::Pid::from_raw(pid as i32)
            .map(|p| rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()))
        {
            tokens.push(
                self.handle
                    .insert_source(Generic::new(fd, Interest::READ, Mode::Level), |_, _, _| {
                        Ok(PostAction::Remove)
                    })
                    .map_err(|e| e.to_string())?,
            );
        }
        self.kids.insert(
            id,
            Kid {
                pty,
                tokens,
                deadline: None,
            },
        );
        Ok((id, pid))
    }

    fn close(&mut self, id: u64) {
        if let Some(k) = self.kids.get_mut(&id).filter(|k| k.deadline.is_none()) {
            signal(k.pty.pid(), rustix::process::Signal::HUP);
            k.deadline = Some(Instant::now() + GRACE);
        }
    }

    fn reap(&mut self) {
        let now = Instant::now();
        let mut done = Vec::new();
        for (&id, k) in &mut self.kids {
            match k.pty.try_wait() {
                Ok(Some(s)) => done.push((id, exit_code(s))),
                Ok(None) if k.deadline.is_none_or(|d| now < d) => {}
                _ => {
                    signal(k.pty.pid(), rustix::process::Signal::KILL);
                    let _ = k.pty.wait();
                    done.push((id, 128 + 1));
                }
            }
        }
        for (id, code) in done {
            if let Some(k) = self.kids.remove(&id) {
                for t in k.tokens {
                    self.handle.remove(t);
                }
            }
            self.exits.push((id, code));
        }
    }

    fn drain_exits(&mut self) -> Vec<(u64, i32)> {
        std::mem::take(&mut self.exits)
    }

    fn is_idle(&self) -> bool {
        self.kids.is_empty()
    }

    fn poll_hint(&self) -> Option<Duration> {
        self.kids
            .values()
            .any(|k| k.deadline.is_some())
            .then_some(Duration::from_millis(10))
    }
}

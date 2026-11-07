//! TEST-ONLY backend: the Wayland-less daemon (`CTERM_DEBUG_DAEMON_HEADLESS=1`).
//!
//! It exists so integration tests can drive the real daemon code paths (listener, peer check,
//! lock race, exit relay, close → SIGHUP, grant stripping) without a compositor. `open` spawns
//! the PTY and child with no surface and drains the PTY master so the child never blocks on
//! output. It is not a user feature and is not documented as one.
//!
//! It also fakes a Wayland connection for the detach tests: `CTERM_DEBUG_HEADLESS_LOSS_MS=N`
//! loses it N ms after start (a socketpair whose peer is dropped, so the loop fails exactly as on
//! a real loss), and `CTERM_DEBUG_HEADLESS_RECONNECT_MS=M` makes the display reachable again M
//! ms after the loss (unset: never).

use super::store;
use super::{Backend, TermId};
use crate::session_life;
use crucible_term_core::Session;
use crucible_term_core::pty::{Pty, SpawnSpec, exit_code};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// TEST-ONLY: lose the fake Wayland connection N ms after start.
pub const LOSS_VAR: &str = "CTERM_DEBUG_HEADLESS_LOSS_MS";
/// TEST-ONLY: the fake display is reachable again M ms after the loss.
pub const RECONNECT_VAR: &str = "CTERM_DEBUG_HEADLESS_RECONNECT_MS";
/// TEST-ONLY: `1` makes the fake advertise the identity global, so granted opens (and the
/// session-key reattach) are served.
pub const IDENTITY_VAR: &str = "CTERM_DEBUG_HEADLESS_IDENTITY";

struct Kid {
    pty: Pty,
    tokens: Vec<RegistrationToken>,
    /// Set once the child exited (pidfd or PTY EOF), as the Wayland host's `Term::exit`.
    exit: Option<i32>,
    /// Has a (fake) surface; cleared by a loss, set again by [`Backend::attach`].
    attached: bool,
    /// The VT state fed from the PTY drain (so the record store can capture and replay).
    session: Session,
    /// Bumped on every PTY read.
    generation: u64,
    last_output: Instant,
    scrollback: usize,
}

/// The headless backend; the calloop data in headless mode.
pub struct Headless {
    handle: LoopHandle<'static, Headless>,
    next: u64,
    kids: BTreeMap<TermId, Kid>,
    exits: Vec<(TermId, i32)>,
    /// Children in their close grace.
    closing: usize,
    /// The fake connection's source, while connected.
    fake: Option<RegistrationToken>,
    /// When the fake connection was lost.
    lost_at: Option<Instant>,
    /// How long after a loss the fake display is reachable again (`None`: never).
    back_after: Option<Duration>,
    /// Advertise the (fake) identity global.
    pub identity: bool,
}

impl Headless {
    /// A backend inserting its sources through `handle`.
    pub fn new(handle: LoopHandle<'static, Headless>) -> Headless {
        Headless {
            handle,
            next: 1,
            kids: BTreeMap::new(),
            exits: Vec::new(),
            closing: 0,
            fake: None,
            lost_at: None,
            back_after: None,
            identity: false,
        }
    }

    /// Fake a Wayland connection that is lost `loss` after now; reachable again `back_after`
    /// after the loss.
    pub fn fake_wayland(
        &mut self,
        loss: Duration,
        back_after: Option<Duration>,
    ) -> Result<(), String> {
        let (ours, peer) = UnixStream::pair().map_err(|e| e.to_string())?;
        ours.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut ours_r = ours.try_clone().map_err(|e| e.to_string())?;
        let tok = self
            .handle
            .insert_source(
                Generic::new(ours, Interest::READ, Mode::Level),
                move |_, _, _| {
                    let mut b = [0u8; 64];
                    match ours_r.read(&mut b) {
                        Ok(0) => Err(std::io::Error::other("fake Wayland connection lost")),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            Ok(PostAction::Continue)
                        }
                        Ok(_) | Err(_) => Ok(PostAction::Continue),
                    }
                },
            )
            .map_err(|e| e.to_string())?;
        let mut peer_slot = Some(peer);
        self.handle
            .insert_source(Timer::from_duration(loss), move |_, _, _| {
                drop(peer_slot.take());
                TimeoutAction::Drop
            })
            .map_err(|e| e.to_string())?;
        self.fake = Some(tok);
        self.back_after = back_after;
        Ok(())
    }

    /// TEST-ONLY: parse a millisecond env value (unset or unparsable: `None`).
    pub fn parse_ms(v: Option<&str>) -> Option<Duration> {
        v?.trim().parse().ok().map(Duration::from_millis)
    }
}

/// The child of session `id` is gone (pidfd or PTY EOF): reap it and record its code.
fn child_gone(h: &mut Headless, id: TermId) {
    if let Some(k) = h.kids.get_mut(&id).filter(|k| k.exit.is_none()) {
        k.exit = Some(k.pty.wait().map(exit_code).unwrap_or(1));
    }
}

impl Backend for Headless {
    fn has_identity(&self) -> bool {
        self.identity
    }
    fn open(
        &mut self,
        cfg: &crate::window::Config,
        argv: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        _grant: Option<String>,
    ) -> Result<(TermId, u32), String> {
        if self.is_lost() {
            return Err("no Wayland connection".into());
        }
        let pty = Pty::spawn(&SpawnSpec {
            argv,
            cwd: cfg.cwd.clone(),
            env,
            size: (cfg.cols, cfg.rows),
        })
        .map_err(|e| format!("pty: {e}"))?;
        let id = TermId(self.next);
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
                .insert_source(drain, move |_, _, h| {
                    let mut buf = [0u8; 4096];
                    loop {
                        match master.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                if let Some(k) = h.kids.get_mut(&id) {
                                    k.session.feed(&buf[..n]);
                                    let _ = k.session.take_replies();
                                    let _ = k.session.take_events();
                                    k.generation += 1;
                                    k.last_output = Instant::now();
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                return Ok(PostAction::Continue);
                            }
                            Err(_) => break,
                        }
                    }
                    child_gone(h, id);
                    Ok(PostAction::Remove)
                })
                .map_err(|e| e.to_string())?,
        );
        tokens.extend(session_life::watch_exit(&self.handle, pid, move |h| {
            child_gone(h, id)
        })?);
        self.kids.insert(
            id,
            Kid {
                pty,
                tokens,
                exit: None,
                attached: true,
                session: Session::new(
                    usize::from(cfg.cols),
                    usize::from(cfg.rows),
                    cfg.settings.scrollback,
                ),
                generation: 0,
                last_output: Instant::now(),
                scrollback: cfg.settings.scrollback,
            },
        );
        Ok((id, pid))
    }

    fn close(&mut self, id: TermId) {
        let Some(k) = self.kids.remove(&id) else {
            return;
        };
        for t in k.tokens {
            self.handle.remove(t);
        }
        if let Some(code) = k.exit {
            self.exits.push((id, code));
            return;
        }
        self.closing += 1;
        let armed = session_life::begin_grace(&self.handle, k.pty, move |h: &mut Headless, c| {
            h.exits.push((id, c));
            h.closing -= 1;
        });
        if !armed {
            self.closing -= 1;
            self.exits.push((id, 128 + 1));
        }
    }

    fn reap(&mut self) {
        let done: Vec<TermId> = self
            .kids
            .iter()
            .filter(|(_, k)| k.exit.is_some())
            .map(|(&id, _)| id)
            .collect();
        for id in done {
            if let Some(k) = self.kids.remove(&id) {
                for t in k.tokens {
                    self.handle.remove(t);
                }
                self.exits.extend(k.exit.map(|c| (id, c)));
            }
        }
    }

    fn drain_exits(&mut self) -> Vec<(TermId, i32)> {
        std::mem::take(&mut self.exits)
    }

    fn is_idle(&self) -> bool {
        self.kids.is_empty() && self.closing == 0
    }

    fn on_lost(&mut self) -> bool {
        let Some(t) = self.fake.take() else {
            return false;
        };
        self.handle.remove(t);
        for k in self.kids.values_mut() {
            k.attached = false;
        }
        self.lost_at = Some(Instant::now());
        eprintln!(
            "crucible-term: Wayland connection lost; {} session(s) detached",
            self.kids.len()
        );
        true
    }

    fn is_lost(&self) -> bool {
        self.lost_at.is_some()
    }

    fn reconnect(&mut self) -> bool {
        let Some(at) = self.lost_at else {
            return true;
        };
        if self.back_after.is_some_and(|d| at.elapsed() >= d) {
            self.lost_at = None;
            eprintln!("crucible-term: Wayland reconnected (fake)");
            return true;
        }
        false
    }

    fn is_detached(&self, id: TermId) -> bool {
        self.kids.get(&id).is_some_and(|k| !k.attached)
    }

    fn attach(&mut self, id: TermId, _grant: Option<String>) -> Result<u32, String> {
        if self.is_lost() {
            return Err("no Wayland connection".into());
        }
        match self.kids.get_mut(&id) {
            Some(k) if !k.attached && k.exit.is_none() => {
                k.attached = true;
                Ok(k.pty.pid())
            }
            Some(_) => Err(format!("session {id} is bound")),
            None => Err(format!("no session {id}")),
        }
    }
    fn view(&self, id: TermId) -> Option<store::View<'_>> {
        self.kids.get(&id).map(|k| store::View {
            session: &k.session,
            generation: k.generation,
            last_output: k.last_output,
            finishing: k.exit.is_some(),
        })
    }

    fn restore(&mut self, id: TermId, rec: &store::Record) -> Option<(usize, u64)> {
        let k = self.kids.get_mut(&id)?;
        let (cols, rows) = k.session.size();
        let (s, n) = store::restored(rec, cols, rows, k.scrollback);
        k.session = s;
        Some((n, k.generation))
    }
}

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
//! ms after the loss (unset: never). `CTERM_DEBUG_HEADLESS_NO_DISPLAY=1` starts it with no
//! connection at all (as the real daemon starts): the first open adopts its display and the fake
//! connects at once.

use super::store;
use super::{Backend, Info, Seen, TermId};
use crate::outbox::Outbox;
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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// TEST-ONLY: lose the fake Wayland connection N ms after start.
pub const LOSS_VAR: &str = "CTERM_DEBUG_HEADLESS_LOSS_MS";
/// TEST-ONLY: the fake display is reachable again M ms after the loss.
pub const RECONNECT_VAR: &str = "CTERM_DEBUG_HEADLESS_RECONNECT_MS";
/// TEST-ONLY: `1` makes the fake advertise the identity global, so granted opens (and the
/// session-key reattach) are served.
pub const IDENTITY_VAR: &str = "CTERM_DEBUG_HEADLESS_IDENTITY";
/// TEST-ONLY: `1` starts with no (fake) connection and no display, so the first open adopts one.
pub const NO_DISPLAY_VAR: &str = "CTERM_DEBUG_HEADLESS_NO_DISPLAY";

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
    /// Bytes for the child (viewer input), written on push, the rest on writable.
    outbox: Outbox,
    /// The writable source draining `outbox`, while registered.
    write_source: Option<RegistrationToken>,
    /// Title, directory and BELs from the event drain.
    seen: Seen,
    /// The size the open asked for: what `set_size(None)` returns to.
    window_size: (u16, u16),
    /// The spawn directory.
    cwd: PathBuf,
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
    /// The display adopted last; a (re)connect needs one.
    display: Option<PathBuf>,
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
            display: None,
        }
    }

    /// Start with no connection: the first open's display is adopted and reachable at once.
    pub fn start_without_display(&mut self) {
        self.lost_at = Some(Instant::now());
        self.back_after = Some(Duration::ZERO);
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

/// Write what the master accepts of session `id`'s outbox; true when nothing is left to wait
/// for (drained, or a hard error that dropped the queue).
fn flush_input(h: &mut Headless, id: TermId) -> bool {
    let Some(k) = h.kids.get_mut(&id) else {
        return true;
    };
    let mut master: &std::fs::File = k.pty.master();
    match k.outbox.flush(&mut master) {
        Ok(done) => done,
        Err(e) => {
            eprintln!("crucible-term: pty write: {e}");
            k.outbox.clear();
            true
        }
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
                    // Bounded per wake-up (level-triggered: the rest comes next dispatch), so a
                    // flood never starves the loop's other sources.
                    let mut total = 0;
                    loop {
                        if total >= 1 << 20 {
                            return Ok(PostAction::Continue);
                        }
                        match master.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                total += n;
                                if let Some(k) = h.kids.get_mut(&id) {
                                    k.session.feed(&buf[..n]);
                                    let _ = k.session.take_replies();
                                    for ev in k.session.take_events() {
                                        k.seen.note(&ev);
                                    }
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
                outbox: Outbox::default(),
                write_source: None,
                seen: Seen::default(),
                window_size: (cfg.cols, cfg.rows),
                cwd: cfg.cwd.clone(),
            },
        );
        Ok((id, pid))
    }

    fn close(&mut self, id: TermId) {
        let Some(k) = self.kids.remove(&id) else {
            return;
        };
        for t in k.tokens.into_iter().chain(k.write_source) {
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
                for t in k.tokens.into_iter().chain(k.write_source) {
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

    fn display(&self) -> Option<&Path> {
        self.display.as_deref()
    }

    fn set_display(&mut self, display: PathBuf) {
        self.display = Some(display);
    }

    fn reconnect(&mut self) -> bool {
        let Some(at) = self.lost_at else {
            return true;
        };
        let Some(d) = &self.display else {
            return false;
        };
        if self.back_after.is_some_and(|b| at.elapsed() >= b) {
            self.lost_at = None;
            eprintln!("crucible-term: Wayland connected to {} (fake)", d.display());
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

    fn write_input(&mut self, id: TermId, bytes: &[u8]) -> bool {
        let Some(k) = self.kids.get_mut(&id).filter(|k| k.exit.is_none()) else {
            return false;
        };
        if bytes.is_empty() {
            return true;
        }
        k.outbox.push(bytes);
        if k.write_source.is_some() || flush_input(self, id) {
            return true;
        }
        let Some(k) = self.kids.get_mut(&id) else {
            return true;
        };
        let Ok(fd) = k.pty.master().try_clone() else {
            k.outbox.clear();
            return true;
        };
        let r = self.handle.insert_source(
            Generic::new(fd, Interest::WRITE, Mode::Level),
            move |_, _, h: &mut Headless| {
                if flush_input(h, id) {
                    if let Some(k) = h.kids.get_mut(&id) {
                        k.write_source = None;
                    }
                    Ok(PostAction::Remove)
                } else {
                    Ok(PostAction::Continue)
                }
            },
        );
        match r {
            Ok(t) => k.write_source = Some(t),
            Err(e) => {
                eprintln!("crucible-term: pty writable source: {e}");
                k.outbox.clear();
            }
        }
        true
    }

    fn set_size(&mut self, id: TermId, size: Option<(u16, u16)>) -> Result<(), String> {
        let k = self
            .kids
            .get_mut(&id)
            .ok_or_else(|| format!("no session {id}"))?;
        let (cols, rows) = size.unwrap_or(k.window_size);
        if k.session.size() != (usize::from(cols), usize::from(rows)) {
            k.session.resize(usize::from(cols), usize::from(rows));
            k.pty.resize((cols, rows)).map_err(|e| e.to_string())?;
            k.generation += 1;
        }
        Ok(())
    }

    fn info(&self, id: TermId) -> Option<Info> {
        self.kids.get(&id).map(|k| Info {
            pid: k.pty.pid(),
            title: k.seen.title.clone(),
            cwd: k.seen.cwd.clone().unwrap_or_else(|| k.cwd.clone()),
        })
    }

    fn take_bells(&mut self, id: TermId) -> u32 {
        self.kids
            .get_mut(&id)
            .map_or(0, |k| std::mem::take(&mut k.seen.bells))
    }
}

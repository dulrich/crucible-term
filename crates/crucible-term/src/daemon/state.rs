//! The daemon's registry: connections and one [`SessionRec`] per live session, plus the
//! `detached-timeout` / orphan-grace clock ([`detached_clock`]) that runs over it.

use super::{Backend, TermId};
use crate::wire::{self, Msg};
use smithay_client_toolkit::reexports::calloop::RegistrationToken;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub(super) struct Conn {
    pub(super) stream: UnixStream,
    pub(super) buf: Vec<u8>,
    pub(super) hello: bool,
    /// An `open` is waiting on the test-only delay timer.
    pub(super) opening: bool,
    pub(super) token: Option<RegistrationToken>,
}

/// When a session is closed unless something happens first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Expiry {
    /// Its client hung up while connected; a loss inside [`super::ORPHAN_GRACE`] clears it.
    Orphan(Instant),
    /// `detached-timeout`: set at reconnect on a detached session, cleared by a reattach or a loss.
    Detached(Instant),
}

/// One live session (removed when its exit is drained).
#[derive(Default)]
pub(super) struct SessionRec {
    /// The owning connection (`None`: its client hung up, or it was never re-owned).
    pub(super) conn: Option<u64>,
    /// Session key it can be reattached under (registered only for granted opens).
    pub(super) key: Option<String>,
    pub(super) expiry: Option<Expiry>,
    /// The durable record this session owns (`scrollback-persist`). Kept apart from `key`: a
    /// detached timeout clears the key, and the record still gets its final capture and its
    /// delayed deletion.
    pub(super) record: Option<super::store::Owned>,
}

impl std::fmt::Display for TermId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) conns: BTreeMap<u64, Conn>,
    pub(super) sessions: BTreeMap<TermId, SessionRec>,
    pub(super) next_conn: u64,
    /// TEST-ONLY delay still to apply to the first granted `open`.
    pub(super) delay: Option<Duration>,
    /// `detached-timeout` (`None`: never).
    pub(super) detached_timeout: Option<Duration>,
    /// Session records (`scrollback-persist`), when on.
    pub(super) persist: Option<super::store::Persist>,
    /// SIGTERM or SIGHUP arrived (persistence on): save and exit.
    pub(super) shutdown: Option<i32>,
}

impl State {
    pub(super) fn send(&mut self, conn: u64, m: &Msg) {
        if let Some(c) = self.conns.get_mut(&conn) {
            let _ = c.stream.write_all(&wire::encode(m));
        }
    }

    /// Capture session `id`'s record now if it changed (before the backend drops its session).
    pub(super) fn final_capture<B: Backend>(&mut self, b: &B, id: TermId) {
        if let (Some(p), Some(r)) = (&self.persist, self.sessions.get_mut(&id))
            && let (Some(o), Some(v)) = (r.record.as_mut(), b.view(id))
        {
            p.tick(o, &v, Instant::now(), true);
        }
    }

    /// The first session whose record matches `f`.
    pub(super) fn find(&self, f: impl Fn(&SessionRec) -> bool) -> Option<TermId> {
        self.sessions.iter().find(|(_, r)| f(r)).map(|(&id, _)| id)
    }
}

/// The `detached-timeout` state machine, run after every dispatch:
/// - lost (connected → lost): every pending expiry (detached or orphan) is cancelled;
/// - reconnected (lost → connected): every live session still detached gets `now + timeout`;
/// - a deadline that passes on a still-detached session closes it (SIGHUP grace) and drops its
///   key (never reattached); a passed orphan expiry closes a session still attached and unowned.
///
/// Both closes take the session's final record capture first; the record identity
/// (`SessionRec::record`) survives the cleared key, and both count as real closes (a timeout is an
/// abandoned session, an orphan a client that died while the display was up), so the record is
/// deleted after the grace unless a shutdown signal arrives first.
pub(super) fn detached_clock<B: Backend>(
    st: &mut State,
    b: &mut B,
    was_lost: bool,
    lost: bool,
    now: Instant,
) {
    let timeout = st.detached_timeout;
    let mut close = Vec::new();
    for (&id, r) in &mut st.sessions {
        let detached = b.is_detached(id);
        match r.expiry {
            _ if lost => r.expiry = None,
            None if was_lost && detached => r.expiry = timeout.map(|t| Expiry::Detached(now + t)),
            Some(Expiry::Detached(d)) if d <= now => {
                r.expiry = None;
                if detached {
                    eprintln!(
                        "crucible-term: daemon: detached session {id} unclaimed after {}s; closing",
                        timeout.map_or(0, |t| t.as_secs())
                    );
                    r.key = None;
                    close.push(id);
                }
            }
            Some(Expiry::Orphan(d)) if d <= now => {
                r.expiry = None;
                if !detached && r.conn.is_none() {
                    close.push(id);
                }
            }
            _ => {}
        }
    }
    for id in close {
        st.final_capture(b, id);
        b.close(id);
    }
}

#[cfg(test)]
mod clock_tests {
    use super::super::ORPHAN_GRACE;
    use super::*;
    use crate::window;
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    /// Sessions 1 and 2; `detached` holds the surface-less ones; `closed` records `close`.
    #[derive(Default)]
    struct Fake {
        detached: BTreeSet<TermId>,
        closed: Vec<TermId>,
    }

    impl Backend for Fake {
        fn open(
            &mut self,
            _: &window::Config,
            _: Vec<OsString>,
            _: Vec<(OsString, OsString)>,
            _: Option<String>,
        ) -> Result<(TermId, u32), String> {
            Err("fake".into())
        }
        fn close(&mut self, id: TermId) {
            self.closed.push(id);
        }
        fn reap(&mut self) {}
        fn drain_exits(&mut self) -> Vec<(TermId, i32)> {
            Vec::new()
        }
        fn is_idle(&self) -> bool {
            false
        }
        fn is_detached(&self, id: TermId) -> bool {
            self.detached.contains(&id)
        }
        fn attach(&mut self, id: TermId, _: Option<String>) -> Result<u32, String> {
            self.detached.remove(&id);
            Ok(1)
        }
    }

    const S1: TermId = TermId(1);
    const S2: TermId = TermId(2);

    /// A state with live sessions 1 and 2.
    fn two() -> State {
        State {
            sessions: [(S1, SessionRec::default()), (S2, SessionRec::default())].into(),
            ..State::default()
        }
    }

    fn detached_count(st: &State) -> usize {
        st.sessions
            .values()
            .filter(|r| matches!(r.expiry, Some(Expiry::Detached(_))))
            .count()
    }

    fn orphan_count(st: &State) -> usize {
        st.sessions
            .values()
            .filter(|r| matches!(r.expiry, Some(Expiry::Orphan(_))))
            .count()
    }

    #[test]
    fn the_detached_timeout_runs_from_reconnect_and_a_loss_cancels_it() {
        let t = Duration::from_secs(30);
        let mut st = State {
            detached_timeout: Some(t),
            ..two()
        };
        st.sessions.get_mut(&S1).unwrap().key = Some("k".into());
        let mut b = Fake {
            detached: [S1, S2].into(),
            ..Fake::default()
        };
        let t0 = Instant::now();
        // While lost, no clock runs.
        detached_clock(&mut st, &mut b, false, true, t0);
        assert_eq!(detached_count(&st), 0);
        // Reconnect arms both; a loss before expiry cancels them; the next reconnect re-arms.
        detached_clock(&mut st, &mut b, true, false, t0);
        assert_eq!(detached_count(&st), 2);
        detached_clock(&mut st, &mut b, false, true, t0 + t / 2);
        assert_eq!(detached_count(&st), 0);
        let t1 = t0 + t;
        detached_clock(&mut st, &mut b, true, false, t1);
        assert_eq!(st.sessions[&S1].expiry, Some(Expiry::Detached(t1 + t)));
        // Session 2 is reattached (its deadline removed as `try_reattach` does).
        b.detached.remove(&S2);
        st.sessions.get_mut(&S2).unwrap().expiry = None;
        detached_clock(
            &mut st,
            &mut b,
            false,
            false,
            t1 + t - Duration::from_millis(1),
        );
        assert!(b.closed.is_empty());
        detached_clock(&mut st, &mut b, false, false, t1 + t);
        assert_eq!(b.closed, [S1]);
        assert_eq!(
            st.find(|r| r.key.as_deref() == Some("k")),
            None,
            "an expired session can never be reattached"
        );
    }

    /// A client hang-up while connected closes its session only after `ORPHAN_GRACE`; a loss inside
    /// the grace (a compositor quit taking the thin clients with it) keeps the session.
    #[test]
    fn an_orphan_closes_after_the_grace_unless_the_connection_is_lost_first() {
        let mut st = two();
        let mut b = Fake::default();
        let t0 = Instant::now();
        st.sessions.get_mut(&S1).unwrap().expiry = Some(Expiry::Orphan(t0 + ORPHAN_GRACE));
        detached_clock(&mut st, &mut b, false, false, t0 + ORPHAN_GRACE / 2);
        assert!(b.closed.is_empty(), "not before the grace");
        detached_clock(&mut st, &mut b, false, false, t0 + ORPHAN_GRACE);
        assert_eq!(b.closed, [S1]);
        assert_eq!(orphan_count(&st), 0);
        // Session 2: the connection is lost inside the grace, so it is detached, not closed.
        st.sessions.get_mut(&S2).unwrap().expiry = Some(Expiry::Orphan(t0 + ORPHAN_GRACE));
        b.detached.insert(S2);
        detached_clock(&mut st, &mut b, false, true, t0 + ORPHAN_GRACE / 2);
        assert_eq!(orphan_count(&st), 0);
        detached_clock(&mut st, &mut b, true, false, t0 + ORPHAN_GRACE * 2);
        assert_eq!(b.closed, [S1], "the detached session waits for reattach");
    }
}

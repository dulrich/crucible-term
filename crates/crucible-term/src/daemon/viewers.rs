//! Daemon viewers: connections attached to a session that receive rendered frames for their own
//! terminal size ([`crucible_term_core::Viewer`]) and send input and resizes (the `Attach` family
//! in [`crate::wire`]).
//!
//! **Output is a real write path.** Every viewer-protocol reply goes through the connection's
//! [`Out`] queue (control and paint bytes in one queue, in order), written without blocking; what
//! the socket does not take waits for a writable source the loop registers ([`sync_writers`]).
//! A frame is committed as the viewer's diff baseline only once its last byte is written; while a
//! viewer's queue is non-empty no new frame is encoded for it, so a stalled viewer holds at most
//! what was queued before it stalled, and its next frame is diffed from what it really shows.
//! Frames to one viewer are at most one per [`COALESCE`]; a queue undrained for [`STALL`] drops
//! the connection. Window clients keep their direct writes (`State::send`).
//!
//! **Size authority**: one owner per session, the window or one viewer ([`Owner`]). `Attach` and
//! `Input` promote the sender; a window open or rebind promotes the window ([`window_active`]);
//! `Resize` changes the session only when the sender owns it. A viewer never owns a session's
//! life: its hang-up, detach or drop closes nothing.

use super::state::{Expiry, State};
use super::{Backend, TermId};
use crate::wire::{self, Msg};
use crucible_term_core::model::Event;
use crucible_term_core::{Frame, Viewer};
use smithay_client_toolkit::reexports::calloop::RegistrationToken;
use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// At most one frame per viewer per this long.
pub(super) const COALESCE: Duration = Duration::from_millis(8);
/// A viewer whose queue stays undrained this long is dropped.
pub(super) const STALL: Duration = Duration::from_secs(30);
/// TEST-ONLY: override [`STALL`] (milliseconds).
pub const STALL_VAR: &str = "CTERM_DEBUG_VIEWER_STALL_MS";
/// Most BELs relayed in one go (a BEL flood is not worth more than this).
const MAX_BELLS: u32 = 16;

/// What a session's event drain recorded for its viewers.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// The current OSC 0/2 title (empty: unset).
    pub title: String,
    /// The last OSC 7 directory.
    pub cwd: Option<PathBuf>,
    /// BELs not yet taken.
    pub bells: u32,
}

impl Seen {
    /// Record `ev` (titles, directories and BELs; everything else is ignored).
    pub fn note(&mut self, ev: &Event) {
        match ev {
            Event::Title(t) => self.title = t.clone(),
            Event::Cwd(p) => self.cwd = Some(p.clone()),
            Event::Bell => self.bells = self.bells.saturating_add(1),
            _ => {}
        }
    }
}

/// A session as a `Sessions` entry and a viewer title see it.
#[derive(Clone, Debug)]
pub struct Info {
    /// The child's pid.
    pub pid: u32,
    /// The current title (empty: unset).
    pub title: String,
    /// The working directory (the last OSC 7, else the spawn one).
    pub cwd: PathBuf,
}

/// Who sizes a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Owner {
    /// The window (or nobody yet).
    #[default]
    Window,
    /// The viewer on this connection.
    Viewer(u64),
}

/// A connection's outgoing queue for viewer-protocol messages.
#[derive(Default)]
pub(super) struct Out {
    buf: VecDeque<u8>,
    /// Bytes ever queued / ever written (a frame commits once `written` reaches its end).
    queued: u64,
    written: u64,
    /// Since when queued bytes wait for the socket (cleared when drained).
    pub(super) stalled_since: Option<Instant>,
    /// Close the connection once drained (a `Detach`, a child exit, a `Kill` reply).
    pub(super) close_when_drained: bool,
    /// The writable source, while registered.
    pub(super) write_token: Option<RegistrationToken>,
}

impl Out {
    pub(super) fn push(&mut self, m: &Msg) {
        let f = wire::encode(m);
        self.queued += f.len() as u64;
        self.buf.extend(f);
    }

    /// `bytes` as `Output` frames of at most [`wire::OUTPUT_CHUNK`].
    pub(super) fn output(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(wire::OUTPUT_CHUNK) {
            self.push(&Msg::Output { bytes: c.to_vec() });
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Write what `s` takes now. `Ok(true)`: drained.
    pub(super) fn flush(&mut self, mut s: &UnixStream, now: Instant) -> std::io::Result<bool> {
        while !self.buf.is_empty() {
            let (front, _) = self.buf.as_slices();
            match s.write(front) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.buf.drain(..n);
                    self.written += n as u64;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.stalled_since.get_or_insert(now);
                    return Ok(false);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.stalled_since = None;
        Ok(true)
    }
}

/// An attached viewer (on its connection).
pub(super) struct Watch {
    pub(super) session: TermId,
    viewer: Viewer,
    /// The viewer's terminal size, as it last reported it.
    pub(super) size: (u16, u16),
    /// Last `Attach` or `Input` (the hand-over picks the most recent).
    pub(super) last_active: Instant,
    /// The encoded frame waiting to be written in full, and the `Out::queued` mark it ends at.
    pending: Option<(Frame, u64)>,
    /// The output generation and session size of the last encoded frame.
    seen: Option<(u64, (usize, usize))>,
    /// The viewer resized: the next frame is due whatever the session did.
    dirty: bool,
    /// Earliest next encode.
    next_frame: Instant,
    /// A title to send once the queue is drained.
    title: Option<String>,
    bells: u32,
}

impl Watch {
    /// Commit the pending frame if `out` has written it in full.
    fn settle(&mut self, out: &Out) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, at)| out.written >= *at)
            && let Some((f, _)) = self.pending.take()
        {
            self.viewer.commit(f);
        }
    }
}

/// Resolve a session token against `(id, key)` pairs: a decimal id, or a unique prefix of at
/// least [`wire::MIN_KEY_PREFIX`] hex digits of a key. An id of one session that is also a key
/// prefix of another is ambiguous. Errors are the wire's refusal reasons.
pub(super) fn resolve<'a>(
    token: &str,
    sessions: impl IntoIterator<Item = (u64, Option<&'a str>)>,
) -> Result<u64, &'static str> {
    let as_id = (!token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()))
        .then(|| token.parse::<u64>().ok())
        .flatten();
    let prefix = (token.len() >= wire::MIN_KEY_PREFIX
        && token.bytes().all(|b| b.is_ascii_hexdigit()))
    .then(|| token.to_ascii_lowercase());
    let mut hits: Vec<u64> = Vec::new();
    for (id, key) in sessions {
        let by_id = as_id == Some(id);
        let by_key = matches!((&prefix, key), (Some(p), Some(k)) if k.starts_with(p.as_str()));
        if (by_id || by_key) && !hits.contains(&id) {
            hits.push(id);
        }
    }
    match hits[..] {
        [] => Err("no-session"),
        [id] => Ok(id),
        _ => Err("ambiguous"),
    }
}

/// `cols` x `rows` within `1..=MAX_GEOMETRY`.
pub(super) fn geometry(cols: u32, rows: u32) -> Option<(u16, u16)> {
    let ok = |n: u32| (1..=wire::MAX_GEOMETRY).contains(&n);
    (ok(cols) && ok(rows)).then_some((cols as u16, rows as u16))
}

/// OSC 2 for `title`, control characters stripped.
fn osc2(title: &str) -> Vec<u8> {
    let clean: String = title.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]2;{clean}\x1b\\").into_bytes()
}

fn resolve_in<B: Backend>(st: &State, b: &B, token: &str) -> Result<TermId, &'static str> {
    let live = st
        .sessions
        .iter()
        .filter(|(id, _)| b.view(**id).is_some_and(|v| !v.finishing))
        .map(|(id, r)| (id.0, r.key.as_deref()));
    resolve(token, live).map(TermId)
}

/// Queue `m` on `conn`'s viewer queue.
fn reply(st: &mut State, conn: u64, m: &Msg) {
    if let Some(c) = st.conns.get_mut(&conn) {
        c.out.push(m);
    }
}

fn refuse(st: &mut State, conn: u64, reason: &str) {
    reply(
        st,
        conn,
        &Msg::Refused {
            reason: reason.into(),
        },
    );
}

/// "The window was active": session `id`'s size goes back to its window. Called on a window
/// open or rebind; the window's own input reports it here too (WP-5).
pub(super) fn window_active<B: Backend>(st: &mut State, b: &mut B, id: TermId) {
    if let Some(r) = st.sessions.get_mut(&id) {
        r.owner = Owner::Window;
        let _ = b.set_size(id, None);
    }
}

/// `conn`'s viewer takes over its session's size. A refused request (a window that keeps its
/// size) leaves the viewer seeing the session clipped.
fn promote<B: Backend>(st: &mut State, b: &mut B, conn: u64, now: Instant) {
    let Some(w) = st.conns.get_mut(&conn).and_then(|c| c.watch.as_mut()) else {
        return;
    };
    w.last_active = now;
    let (id, size) = (w.session, w.size);
    let Some(r) = st.sessions.get_mut(&id) else {
        return;
    };
    if r.owner != Owner::Viewer(conn) {
        r.owner = Owner::Viewer(conn);
        let _ = b.set_size(id, Some(size));
    }
}

/// `conn`'s viewer leaves its session (detach, hang-up, drop): the size goes back to the window,
/// else to the most recently active remaining viewer, and the last viewer leaving a detached
/// session re-arms `detached-timeout`. Returns the viewer (for its leave sequence).
pub(super) fn leave<B: Backend>(st: &mut State, b: &mut B, conn: u64) -> Option<Viewer> {
    let w = st.conns.get_mut(&conn)?.watch.take()?;
    let id = w.session;
    let timeout = st.detached_timeout;
    let r = st.sessions.get_mut(&id)?;
    r.viewers.retain(|&v| v != conn);
    if r.viewers.is_empty()
        && r.expiry.is_none()
        && let Some(t) = timeout
        && b.is_detached(id)
        && !b.is_lost()
    {
        r.expiry = Some(Expiry::Detached(Instant::now() + t));
    }
    if r.owner == Owner::Viewer(conn) {
        let next = r
            .viewers
            .iter()
            .filter_map(|v| {
                let w = st.conns.get(v)?.watch.as_ref()?;
                Some((w.last_active, *v))
            })
            .max()
            .map(|(_, v)| v);
        match next {
            _ if !b.is_detached(id) => window_active(st, b, id),
            Some(v) => {
                if let Some(r) = st.sessions.get_mut(&id) {
                    r.owner = Owner::Window;
                }
                promote(st, b, v, Instant::now());
            }
            None => r.owner = Owner::Window,
        }
    }
    Some(w.viewer)
}

/// Handle a viewer-protocol message from `conn` (after `hello`). Returns false to drop it.
pub(super) fn on_msg<B: Backend>(st: &mut State, b: &mut B, conn: u64, m: Msg) -> bool {
    let Some(c) = st.conns.get(&conn) else {
        return false;
    };
    if c.out.close_when_drained {
        return true;
    }
    let viewing = c.watch.is_some();
    let now = Instant::now();
    match m {
        Msg::List => {
            let entries = st
                .sessions
                .iter()
                .filter_map(|(&id, r)| {
                    let v = b.view(id)?;
                    let i = b.info(id)?;
                    let (cols, rows) = v.session.size();
                    Some(wire::Entry {
                        id: id.0,
                        key: r.key.clone(),
                        pid: i.pid,
                        title: wire::truncate(&i.title, wire::MAX_FIELD),
                        cwd: wire::truncate(&i.cwd.to_string_lossy(), wire::MAX_FIELD),
                        cols: cols as u32,
                        rows: rows as u32,
                        has_window: !b.is_detached(id),
                        viewers: r.viewers.len() as u32,
                    })
                })
                .take(wire::MAX_ENTRIES)
                .collect();
            reply(st, conn, &Msg::Sessions { entries });
        }
        Msg::Attach {
            session,
            cols,
            rows,
        } => {
            if viewing || c.opening || st.find(|r| r.conn == Some(conn)).is_some() {
                refuse(st, conn, "already-attached");
                return true;
            }
            let Some(size) = geometry(cols, rows) else {
                refuse(st, conn, "bad-size");
                return true;
            };
            let id = match resolve_in(st, b, &session) {
                Ok(id) => id,
                Err(reason) => {
                    refuse(st, conn, reason);
                    return true;
                }
            };
            let title = b.info(id).map(|i| i.title).unwrap_or_default();
            // BELs rung while nobody was viewing are not replayed to the first viewer.
            if st.sessions.get(&id).is_some_and(|r| r.viewers.is_empty()) {
                let _ = b.take_bells(id);
            }
            let Some(r) = st.sessions.get_mut(&id) else {
                refuse(st, conn, "no-session");
                return true;
            };
            r.viewers.push(conn);
            if matches!(r.expiry, Some(Expiry::Detached(_))) {
                r.expiry = None;
            }
            // The session's title as the viewers know it; others hear of a change by the pump.
            let others: Vec<u64> = if r.title != title {
                r.viewers.clone()
            } else {
                Vec::new()
            };
            r.title.clone_from(&title);
            for v in others.into_iter().filter(|&v| v != conn) {
                if let Some(w) = st.conns.get_mut(&v).and_then(|c| c.watch.as_mut()) {
                    w.title = Some(title.clone());
                }
            }
            if let Some(c) = st.conns.get_mut(&conn) {
                c.watch = Some(Watch {
                    session: id,
                    viewer: Viewer::new(usize::from(size.0), usize::from(size.1)),
                    size,
                    last_active: now,
                    pending: None,
                    seen: None,
                    dirty: true,
                    next_frame: now,
                    title: None,
                    bells: 0,
                });
            }
            promote(st, b, conn, now);
            let (cols, rows) = b.view(id).map_or((0, 0), |v| v.session.size());
            eprintln!(
                "crucible-term: daemon: client {conn}: viewing session {id} at {cols}x{rows}"
            );
            if let Some(c) = st.conns.get_mut(&conn) {
                c.out.push(&Msg::Attached {
                    id: id.0,
                    cols: cols as u32,
                    rows: rows as u32,
                });
                let mut hello = Viewer::enter();
                if !title.is_empty() {
                    hello.extend(osc2(&title));
                }
                c.out.output(&hello);
            }
        }
        Msg::Input { bytes } => {
            if let Some(id) = c.watch.as_ref().map(|w| w.session) {
                b.write_input(id, &bytes);
                promote(st, b, conn, now);
            }
        }
        Msg::Resize { cols, rows } => {
            let Some(size) = geometry(cols, rows) else {
                return true;
            };
            let Some(w) = st.conns.get_mut(&conn).and_then(|c| c.watch.as_mut()) else {
                return true;
            };
            w.size = size;
            w.viewer.resize(usize::from(size.0), usize::from(size.1));
            w.dirty = true;
            let id = w.session;
            if st
                .sessions
                .get(&id)
                .is_some_and(|r| r.owner == Owner::Viewer(conn))
            {
                let _ = b.set_size(id, Some(size));
            }
        }
        Msg::Detach => {
            if let Some(v) = leave(st, b, conn)
                && let Some(c) = st.conns.get_mut(&conn)
            {
                c.out.output(&v.leave());
                c.out.close_when_drained = true;
            }
        }
        Msg::Kill { session } => match resolve_in(st, b, &session) {
            Ok(id) => {
                eprintln!("crucible-term: daemon: client {conn}: kill session {id}");
                if let Some(r) = st.sessions.get_mut(&id) {
                    r.killers.push(conn);
                }
                st.final_capture(b, id);
                b.close(id);
            }
            Err(reason) => refuse(st, conn, reason),
        },
        other => {
            eprintln!("crucible-term: daemon: client {conn}: unexpected {other:?}");
            return false;
        }
    }
    true
}

/// Session `id`'s child exited with `code`: every viewer gets the leave sequence and `Exited`,
/// every connection that killed it (and is not also its viewer or window) one `Exited`; all of
/// them close once drained.
pub(super) fn on_exit(
    st: &mut State,
    viewers: &[u64],
    killers: &[u64],
    window: Option<u64>,
    code: i32,
) {
    for &v in viewers {
        if let Some(c) = st.conns.get_mut(&v)
            && let Some(w) = c.watch.take()
        {
            c.out.output(&w.viewer.leave());
            c.out.push(&Msg::Exited { code });
            c.out.close_when_drained = true;
        }
    }
    for &k in killers {
        if viewers.contains(&k) || window == Some(k) {
            continue;
        }
        // A connection that still owns a window (of another session) is never closed here.
        let owns = st.find(|r| r.conn == Some(k)).is_some();
        if let Some(c) = st.conns.get_mut(&k) {
            c.out.push(&Msg::Exited { code });
            c.out.close_when_drained = !owns;
        }
    }
}

/// After a dispatch: relay titles and BELs, encode a frame for every viewer whose session changed
/// (coalesced, only into an empty queue), and write what every queue's socket takes now.
/// Returns the connections whose socket failed for good.
pub(super) fn pump<B: Backend>(st: &mut State, b: &mut B, now: Instant) -> Vec<u64> {
    for (&id, r) in &mut st.sessions {
        if r.viewers.is_empty() {
            continue;
        }
        let bells = b.take_bells(id);
        let title = b.info(id).map(|i| i.title);
        let retitle = title.filter(|t| *t != r.title);
        for v in &r.viewers {
            let Some(w) = st.conns.get_mut(v).and_then(|c| c.watch.as_mut()) else {
                continue;
            };
            w.bells = w.bells.saturating_add(bells);
            if let Some(t) = &retitle {
                w.title = Some(t.clone());
            }
        }
        if let Some(t) = retitle {
            r.title = t;
        }
    }
    let mut dead = Vec::new();
    for (&conn, c) in &mut st.conns {
        if let Some(w) = c.watch.as_mut() {
            w.settle(&c.out);
            if c.out.is_empty()
                && let Some(v) = b.view(w.session)
            {
                let now_seen = (v.generation, v.session.size());
                if (w.dirty || w.seen != Some(now_seen)) && now >= w.next_frame {
                    let f = w.viewer.frame(
                        &v.session.screen(),
                        &v.session.modes(),
                        v.session.cursor_style(),
                    );
                    w.seen = Some(now_seen);
                    w.dirty = false;
                    if !f.is_empty() {
                        c.out.output(f.bytes());
                        w.pending = Some((f, c.out.queued));
                        w.next_frame = now + COALESCE;
                    }
                }
                if let Some(t) = w.title.take() {
                    c.out.output(&osc2(&t));
                }
                if w.bells > 0 {
                    c.out.output(&vec![0x07; w.bells.min(MAX_BELLS) as usize]);
                    w.bells = 0;
                }
            }
        }
        if !c.out.is_empty() && c.out.write_token.is_none() {
            match c.out.flush(&c.stream, now) {
                Ok(_) => {
                    if let Some(w) = c.watch.as_mut() {
                        w.settle(&c.out);
                    }
                }
                Err(e) => {
                    eprintln!("crucible-term: daemon: client {conn}: write: {e}");
                    dead.push(conn);
                }
            }
        }
    }
    dead
}

/// The writable source of `conn` fired: write what the socket takes. `Ok(true)`: drained.
pub(super) fn on_writable(st: &mut State, conn: u64) -> std::io::Result<bool> {
    let Some(c) = st.conns.get_mut(&conn) else {
        return Ok(true);
    };
    let done = c.out.flush(&c.stream, Instant::now())?;
    if let Some(w) = c.watch.as_mut() {
        w.settle(&c.out);
    }
    Ok(done)
}

/// When the viewers next need the loop: a coalesced frame that is due, or a stall limit.
pub(super) fn deadline<B: Backend>(st: &State, b: &B) -> Option<Instant> {
    let stall = st.viewer_stall.unwrap_or(STALL);
    st.conns
        .values()
        .filter_map(|c| {
            let frame = c.watch.as_ref().and_then(|w| {
                let v = b.view(w.session)?;
                let changed = w.dirty || w.seen != Some((v.generation, v.session.size()));
                (changed && c.out.is_empty()).then_some(w.next_frame)
            });
            let stalled = c.out.stalled_since.map(|t| t + stall);
            frame.into_iter().chain(stalled).min()
        })
        .min()
}

/// Connections whose queue stayed undrained past the stall limit.
pub(super) fn stalled(st: &State, now: Instant) -> Vec<u64> {
    let stall = st.viewer_stall.unwrap_or(STALL);
    st.conns
        .iter()
        .filter(|(_, c)| c.out.stalled_since.is_some_and(|t| now >= t + stall))
        .map(|(&k, _)| k)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KA: &str = "ab12cd34000000000000000000000000";
    const KB: &str = "ab12ef56000000000000000000000000";

    fn table() -> Vec<(u64, Option<&'static str>)> {
        vec![
            (1, Some(KA)),
            (2, Some(KB)),
            (3, None),
            (12, Some("1234abcd")),
        ]
    }

    #[test]
    fn tokens_resolve_by_id_or_unique_key_prefix_and_never_guess() {
        assert_eq!(resolve("1", table()), Ok(1));
        assert_eq!(resolve("3", table()), Ok(3));
        assert_eq!(resolve("ab12c", table()), Ok(1));
        assert_eq!(resolve("AB12E", table()), Ok(2), "hex is case-blind");
        assert_eq!(resolve("ab12", table()), Err("ambiguous"), "two prefixes");
        // `1234` is not an id here, and the key prefix of session 12: unique.
        assert_eq!(resolve("1234", table()), Ok(12));
        // `12` is session 12's id; too short to be a prefix.
        assert_eq!(resolve("12", table()), Ok(12));
        assert_eq!(resolve("9", table()), Err("no-session"));
        assert_eq!(
            resolve("abc", table()),
            Err("no-session"),
            "prefix too short"
        );
        assert_eq!(resolve("", table()), Err("no-session"));
        assert_eq!(resolve("zzzz", table()), Err("no-session"));
        // An id of one session that is a key prefix of another is ambiguous.
        let t = vec![(1234, None), (2, Some("1234ffff"))];
        assert_eq!(resolve("1234", t), Err("ambiguous"));
        // The same session by both is not.
        let t = vec![(1234, Some("1234ffff"))];
        assert_eq!(resolve("1234", t), Ok(1234));
    }

    #[test]
    fn geometry_is_bounded_and_titles_lose_control_characters() {
        assert_eq!(geometry(80, 24), Some((80, 24)));
        assert_eq!(geometry(1000, 1), Some((1000, 1)));
        assert_eq!(geometry(0, 24), None);
        assert_eq!(geometry(80, 1001), None);
        assert_eq!(osc2("a\x07b\x1bc"), b"\x1b]2;abc\x1b\\");
    }

    #[test]
    fn output_is_chunked_under_the_frame_bound() {
        let mut o = Out::default();
        o.output(&vec![b'x'; wire::OUTPUT_CHUNK * 2 + 1]);
        let bytes: Vec<u8> = o.buf.iter().copied().collect();
        let mut rest = &bytes[..];
        let mut n = 0;
        while let Some((m, used)) = wire::decode(rest).unwrap() {
            let Msg::Output { bytes } = m else {
                panic!("output")
            };
            assert!(bytes.len() <= wire::OUTPUT_CHUNK);
            n += 1;
            rest = &rest[used..];
        }
        assert_eq!(n, 3);
        assert_eq!(o.queued, bytes.len() as u64);
    }
}

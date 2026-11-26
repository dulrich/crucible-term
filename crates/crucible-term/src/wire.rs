//! The client↔daemon wire: pure framing, no I/O.
//!
//! A frame is a `u32` little-endian length (of everything after it), a `u8` tag, then the
//! payload. Payload primitives: `u32`/`i32` little-endian; bytes as a `u32` length plus the
//! bytes (an `OsString` is its raw unix bytes, a `String` is UTF-8); `Option` as a `u8` 0/1 and
//! then the value; a list as a `u32` count and then the items. A frame is capped at
//! [`MAX_FRAME`]. Both sides send [`Msg::Hello`] first.
//!
//! Viewers (protocol 3, tags 8–16, added without a version bump: a daemon that predates them
//! closes the connection on the unknown tag). A viewer connection sends `hello`, then
//! [`Msg::Attach`]; the daemon answers [`Msg::Attached`] (or [`Msg::Refused`]) and then only
//! [`Msg::Output`] frames: the enter sequence, the current title (OSC 2), then rendered frames for
//! the viewer's size, title changes and BELs. [`Msg::Detach`] gets the leave sequence as `Output`
//! and then EOF; a child exit gets the leave sequence and [`Msg::Exited`]. A session token is a
//! decimal session id or a unique prefix (at least [`MIN_KEY_PREFIX`] hex digits) of its key.
//! Refusal reasons for viewer requests are stable identifiers: `no-session` (no match),
//! `ambiguous` (several sessions, or an id of one and a key prefix of another), `bad-size`
//! (geometry 0 or above [`MAX_GEOMETRY`]), `already-attached` (the connection already views or
//! owns a session). Bounds: `Output` payloads are at most [`OUTPUT_CHUNK`] bytes, `Sessions`
//! titles and cwds at most [`MAX_FIELD`] bytes and at most [`MAX_ENTRIES`] entries.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// The wire protocol version; a mismatch makes the client fall back to standalone.
pub const PROTOCOL_VERSION: u32 = 3;
/// Largest accepted frame (length field value), 1 MiB.
pub const MAX_FRAME: usize = 1 << 20;
/// Largest `cols` or `rows` an `Attach` or `Resize` may name.
pub const MAX_GEOMETRY: u32 = 1000;
/// Largest `Output` payload the daemon sends (a longer frame is split).
pub const OUTPUT_CHUNK: usize = 64 * 1024;
/// Longest `title` or `cwd` in a `Sessions` entry (longer ones are cut at a char boundary).
pub const MAX_FIELD: usize = 256;
/// Most entries in one `Sessions` reply.
pub const MAX_ENTRIES: usize = 1024;
/// Fewest hex digits a key prefix in a session token may have.
pub const MIN_KEY_PREFIX: usize = 4;

/// One line of a `Sessions` reply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The session id.
    pub id: u64,
    /// Its session key, if it has one.
    pub key: Option<String>,
    /// The child's pid.
    pub pid: u32,
    /// The current title (OSC 0/2; empty when unset).
    pub title: String,
    /// The last OSC 7 working directory, else the spawn directory.
    pub cwd: String,
    /// The session's grid width.
    pub cols: u32,
    /// The session's grid height.
    pub rows: u32,
    /// A window (surface) shows it.
    pub has_window: bool,
    /// Attached viewers.
    pub viewers: u32,
}

/// `open`: one window for this client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Open {
    /// The child's argv.
    pub argv: Vec<OsString>,
    /// The client-resolved working directory.
    pub cwd: PathBuf,
    /// The child's environment (the client's, minus `CRUCIBLE_WINDOW_GRANT`).
    pub env: Vec<(OsString, OsString)>,
    /// `-o` overrides, raw `key=value`.
    pub overrides: Vec<String>,
    /// `--app-id`.
    pub app_id: Option<String>,
    /// `--title`.
    pub title: Option<String>,
    /// The CDE window grant, if the spawn carried one.
    pub grant: Option<String>,
    /// `CRUCIBLE_TERM_SESSION` (32 lowercase hex), if valid: names a session to reattach.
    pub session_key: Option<String>,
    /// The client's Wayland display socket, absolute (rejected at decode otherwise). The daemon
    /// adopts it when it has no live connection; a live connection to another display refuses
    /// the open `other-display`.
    pub display: PathBuf,
}

/// One message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    /// First message both ways.
    Hello {
        /// [`PROTOCOL_VERSION`] of the sender.
        protocol_version: u32,
        /// The sender's tool-scoped build id.
        build_id: String,
    },
    /// Client → daemon: open a window.
    Open(Open),
    /// Daemon → client: the window is open.
    Opened {
        /// The child's pid.
        root_pid: u32,
        /// The window rebinds an existing detached session (keyed) instead of a new spawn.
        reattached: bool,
    },
    /// Daemon → client: the open was refused (`no-identity`, `no-wayland`, `other-display`, or a
    /// spawn error).
    Refused {
        /// Why.
        reason: String,
    },
    /// Daemon → client: the window's child exited (code, or 128 + signal).
    Exited {
        /// The exit code.
        code: i32,
    },
    /// Client → daemon: close the window.
    Close,
    /// Daemon → client: another client reattached this session; exit 0 silently.
    Superseded,
    /// Client → daemon: list the live sessions (answered by `Sessions`).
    List,
    /// Client → daemon: become a viewer of `session` at `cols` x `rows`.
    Attach {
        /// A session token (decimal id or key prefix).
        session: String,
        /// The viewer's terminal width.
        cols: u32,
        /// The viewer's terminal height.
        rows: u32,
    },
    /// Client → daemon (viewer): bytes for the child.
    Input {
        /// Raw terminal input.
        bytes: Vec<u8>,
    },
    /// Client → daemon (viewer): the viewer's terminal size changed.
    Resize {
        /// Width.
        cols: u32,
        /// Height.
        rows: u32,
    },
    /// Client → daemon (viewer): stop viewing; the daemon sends the leave sequence, then EOF.
    Detach,
    /// Client → daemon: close `session` as a window close does; answered by one `Exited` (when
    /// the child's exit is recorded) or one `Refused`.
    Kill {
        /// A session token.
        session: String,
    },
    /// Daemon → client: the answer to `List`.
    Sessions {
        /// One per live session, by id.
        entries: Vec<Entry>,
    },
    /// Daemon → client: the viewer is attached to session `id`, currently `cols` x `rows`.
    Attached {
        /// The session id.
        id: u64,
        /// The session's grid width.
        cols: u32,
        /// The session's grid height.
        rows: u32,
    },
    /// Daemon → client (viewer): bytes for the viewer's terminal.
    Output {
        /// VT bytes.
        bytes: Vec<u8>,
    },
}

/// A malformed frame.
#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    /// The payload ended inside a field.
    Truncated,
    /// The length field exceeds [`MAX_FRAME`] (or is zero).
    Oversized(usize),
    /// An unknown message tag.
    UnknownTag(u8),
    /// A string field is not UTF-8.
    BadUtf8,
    /// Bytes left over after the message.
    Trailing,
    /// `open.display` is empty or not an absolute path.
    BadDisplay,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated => f.write_str("truncated frame"),
            WireError::Oversized(n) => write!(f, "frame length {n} outside 1..={MAX_FRAME}"),
            WireError::UnknownTag(t) => write!(f, "unknown message tag {t}"),
            WireError::BadUtf8 => f.write_str("string field is not UTF-8"),
            WireError::Trailing => f.write_str("trailing bytes after message"),
            WireError::BadDisplay => f.write_str("open display is not an absolute path"),
        }
    }
}

impl std::error::Error for WireError {}

const T_HELLO: u8 = 1;
const T_OPEN: u8 = 2;
const T_OPENED: u8 = 3;
const T_REFUSED: u8 = 4;
const T_EXITED: u8 = 5;
const T_CLOSE: u8 = 6;
const T_SUPERSEDED: u8 = 7;
const T_LIST: u8 = 8;
const T_ATTACH: u8 = 9;
const T_INPUT: u8 = 10;
const T_RESIZE: u8 = 11;
const T_DETACH: u8 = 12;
const T_KILL: u8 = 13;
const T_SESSIONS: u8 = 14;
const T_ATTACHED: u8 = 15;
const T_OUTPUT: u8 = 16;

struct W(Vec<u8>);

impl W {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn opt(&mut self, s: &Option<String>) {
        match s {
            None => self.0.push(0),
            Some(s) => {
                self.0.push(1);
                self.bytes(s.as_bytes());
            }
        }
    }
}

/// Encode `m` as one complete frame (length prefix included).
pub fn encode(m: &Msg) -> Vec<u8> {
    let mut w = W(vec![0; 4]);
    match m {
        Msg::Hello {
            protocol_version,
            build_id,
        } => {
            w.0.push(T_HELLO);
            w.u32(*protocol_version);
            w.bytes(build_id.as_bytes());
        }
        Msg::Open(o) => {
            w.0.push(T_OPEN);
            w.u32(o.argv.len() as u32);
            for a in &o.argv {
                w.bytes(a.as_bytes());
            }
            w.bytes(o.cwd.as_os_str().as_bytes());
            w.u32(o.env.len() as u32);
            for (k, v) in &o.env {
                w.bytes(k.as_bytes());
                w.bytes(v.as_bytes());
            }
            w.u32(o.overrides.len() as u32);
            for s in &o.overrides {
                w.bytes(s.as_bytes());
            }
            w.opt(&o.app_id);
            w.opt(&o.title);
            w.opt(&o.grant);
            w.opt(&o.session_key);
            w.bytes(o.display.as_os_str().as_bytes());
        }
        Msg::Opened {
            root_pid,
            reattached,
        } => {
            w.0.push(T_OPENED);
            w.u32(*root_pid);
            w.0.push(u8::from(*reattached));
        }
        Msg::Refused { reason } => {
            w.0.push(T_REFUSED);
            w.bytes(reason.as_bytes());
        }
        Msg::Exited { code } => {
            w.0.push(T_EXITED);
            w.0.extend_from_slice(&code.to_le_bytes());
        }
        Msg::Close => w.0.push(T_CLOSE),
        Msg::Superseded => w.0.push(T_SUPERSEDED),
        Msg::List => w.0.push(T_LIST),
        Msg::Attach {
            session,
            cols,
            rows,
        } => {
            w.0.push(T_ATTACH);
            w.bytes(session.as_bytes());
            w.u32(*cols);
            w.u32(*rows);
        }
        Msg::Input { bytes } => {
            w.0.push(T_INPUT);
            w.bytes(bytes);
        }
        Msg::Resize { cols, rows } => {
            w.0.push(T_RESIZE);
            w.u32(*cols);
            w.u32(*rows);
        }
        Msg::Detach => w.0.push(T_DETACH),
        Msg::Kill { session } => {
            w.0.push(T_KILL);
            w.bytes(session.as_bytes());
        }
        Msg::Sessions { entries } => {
            w.0.push(T_SESSIONS);
            w.u32(entries.len() as u32);
            for e in entries {
                w.u64(e.id);
                w.opt(&e.key);
                w.u32(e.pid);
                w.bytes(e.title.as_bytes());
                w.bytes(e.cwd.as_bytes());
                w.u32(e.cols);
                w.u32(e.rows);
                w.0.push(u8::from(e.has_window));
                w.u32(e.viewers);
            }
        }
        Msg::Attached { id, cols, rows } => {
            w.0.push(T_ATTACHED);
            w.u64(*id);
            w.u32(*cols);
            w.u32(*rows);
        }
        Msg::Output { bytes } => {
            w.0.push(T_OUTPUT);
            w.bytes(bytes);
        }
    }
    debug_assert!(
        w.0.len() - 4 <= MAX_FRAME,
        "encoded frame of {} bytes exceeds MAX_FRAME",
        w.0.len() - 4
    );
    let len = (w.0.len() - 4) as u32;
    w.0[..4].copy_from_slice(&len.to_le_bytes());
    w.0
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], WireError> {
        if self.0.len() < n {
            return Err(WireError::Truncated);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, WireError> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn os(&mut self) -> Result<OsString, WireError> {
        Ok(OsString::from_vec(self.bytes()?))
    }
    fn string(&mut self) -> Result<String, WireError> {
        String::from_utf8(self.bytes()?).map_err(|_| WireError::BadUtf8)
    }
    fn opt(&mut self) -> Result<Option<String>, WireError> {
        match self.u8()? {
            0 => Ok(None),
            _ => Ok(Some(self.string()?)),
        }
    }
    /// A list count; each item takes at least 4 bytes, so a count beyond that is truncation
    /// (and never a huge allocation).
    fn count(&mut self) -> Result<usize, WireError> {
        let n = self.u32()? as usize;
        if n > self.0.len() / 4 {
            return Err(WireError::Truncated);
        }
        Ok(n)
    }
}

/// Decode one frame body (tag + payload, without the length prefix).
pub fn decode_body(body: &[u8]) -> Result<Msg, WireError> {
    let mut r = R(body);
    let m = match r.u8()? {
        T_HELLO => Msg::Hello {
            protocol_version: r.u32()?,
            build_id: r.string()?,
        },
        T_OPEN => {
            let mut o = Open::default();
            for _ in 0..r.count()? {
                o.argv.push(r.os()?);
            }
            o.cwd = PathBuf::from(r.os()?);
            for _ in 0..r.count()? {
                let k = r.os()?;
                o.env.push((k, r.os()?));
            }
            for _ in 0..r.count()? {
                o.overrides.push(r.string()?);
            }
            o.app_id = r.opt()?;
            o.title = r.opt()?;
            o.grant = r.opt()?;
            o.session_key = r.opt()?;
            o.display = PathBuf::from(r.os()?);
            if !o.display.is_absolute() {
                return Err(WireError::BadDisplay);
            }
            Msg::Open(o)
        }
        T_OPENED => Msg::Opened {
            root_pid: r.u32()?,
            reattached: r.u8()? != 0,
        },
        T_REFUSED => Msg::Refused {
            reason: r.string()?,
        },
        T_EXITED => Msg::Exited {
            code: r.u32()? as i32,
        },
        T_CLOSE => Msg::Close,
        T_SUPERSEDED => Msg::Superseded,
        T_LIST => Msg::List,
        T_ATTACH => Msg::Attach {
            session: r.string()?,
            cols: r.u32()?,
            rows: r.u32()?,
        },
        T_INPUT => Msg::Input { bytes: r.bytes()? },
        T_RESIZE => Msg::Resize {
            cols: r.u32()?,
            rows: r.u32()?,
        },
        T_DETACH => Msg::Detach,
        T_KILL => Msg::Kill {
            session: r.string()?,
        },
        T_SESSIONS => {
            let mut entries = Vec::new();
            for _ in 0..r.count()? {
                entries.push(Entry {
                    id: r.u64()?,
                    key: r.opt()?,
                    pid: r.u32()?,
                    title: r.string()?,
                    cwd: r.string()?,
                    cols: r.u32()?,
                    rows: r.u32()?,
                    has_window: r.u8()? != 0,
                    viewers: r.u32()?,
                });
            }
            Msg::Sessions { entries }
        }
        T_ATTACHED => Msg::Attached {
            id: r.u64()?,
            cols: r.u32()?,
            rows: r.u32()?,
        },
        T_OUTPUT => Msg::Output { bytes: r.bytes()? },
        t => return Err(WireError::UnknownTag(t)),
    };
    if !r.0.is_empty() {
        return Err(WireError::Trailing);
    }
    Ok(m)
}

/// Decode the first frame of a stream buffer: `Ok(None)` while the frame is still incomplete,
/// else the message and the bytes it consumed. An oversized length is an error at once.
pub fn decode(buf: &[u8]) -> Result<Option<(Msg, usize)>, WireError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(WireError::Oversized(len));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    Ok(Some((decode_body(&buf[4..4 + len])?, 4 + len)))
}

/// `s` cut to at most `max` bytes at a char boundary.
pub fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// The client's verdict on the daemon's `hello`.
#[derive(Debug, PartialEq, Eq)]
pub enum HelloVerdict {
    /// Same protocol and build.
    Ok,
    /// Same protocol, different build: served, logged.
    BuildMismatch,
    /// Different protocol version (or not a hello): fall back to standalone.
    Fallback,
}

/// Judge a peer's first message against our own build id.
pub fn judge_hello(m: &Msg, own_build: &str) -> HelloVerdict {
    match m {
        Msg::Hello {
            protocol_version,
            build_id,
        } if *protocol_version == PROTOCOL_VERSION => {
            if build_id == own_build {
                HelloVerdict::Ok
            } else {
                HelloVerdict::BuildMismatch
            }
        }
        _ => HelloVerdict::Fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn all() -> Vec<Msg> {
        vec![
            Msg::Hello {
                protocol_version: PROTOCOL_VERSION,
                build_id: "abc-dirty".into(),
            },
            Msg::Open(Open {
                argv: vec!["sh".into(), OsString::from_vec(vec![0xff, b'x'])],
                cwd: "/tmp".into(),
                env: vec![("A".into(), "1".into()), ("B".into(), "".into())],
                overrides: vec!["pad=6x6 center".into()],
                app_id: Some("a.b".into()),
                title: None,
                grant: Some("g".into()),
                session_key: Some("0123456789abcdef0123456789abcdef".into()),
                display: "/run/user/1000/wayland-1".into(),
            }),
            Msg::Open(Open {
                display: "/d".into(),
                ..Open::default()
            }),
            Msg::Opened {
                root_pid: 4242,
                reattached: true,
            },
            Msg::Opened {
                root_pid: 1,
                reattached: false,
            },
            Msg::Refused {
                reason: "no-identity".into(),
            },
            Msg::Exited { code: -1 },
            Msg::Exited { code: 143 },
            Msg::Close,
            Msg::Superseded,
            Msg::List,
            Msg::Attach {
                session: "ab12".into(),
                cols: 80,
                rows: 24,
            },
            Msg::Input {
                bytes: vec![0, 0x1b, b'x', 0xff],
            },
            Msg::Resize {
                cols: 1000,
                rows: 1,
            },
            Msg::Detach,
            Msg::Kill {
                session: "7".into(),
            },
            Msg::Sessions { entries: vec![] },
            Msg::Sessions {
                entries: vec![
                    Entry {
                        id: 1 << 40,
                        key: Some("0123456789abcdef0123456789abcdef".into()),
                        pid: 99,
                        title: "vim é".into(),
                        cwd: "/home/x".into(),
                        cols: 80,
                        rows: 24,
                        has_window: true,
                        viewers: 2,
                    },
                    Entry::default(),
                ],
            },
            Msg::Attached {
                id: 3,
                cols: 200,
                rows: 50,
            },
            Msg::Output {
                bytes: b"\x1b[H".to_vec(),
            },
        ]
    }

    #[test]
    fn viewer_tags_keep_their_numbers_and_truncate_cuts_at_a_char_boundary() {
        // The numbers are the contract with older and newer peers.
        assert_eq!(encode(&Msg::List)[4], 8);
        assert_eq!(encode(&Msg::Detach)[4], 12);
        assert_eq!(
            encode(&Msg::Output { bytes: vec![] })[4..],
            [16, 0, 0, 0, 0]
        );
        assert_eq!(truncate("aé", 2), "a");
        assert_eq!(truncate("abc", 256), "abc");
    }

    #[test]
    fn every_message_round_trips_and_streams() {
        let mut stream = Vec::new();
        for m in all() {
            let f = encode(&m);
            assert_eq!(decode(&f), Ok(Some((m.clone(), f.len()))));
            stream.extend(f);
        }
        let mut got = Vec::new();
        let mut buf = &stream[..];
        while let Some((m, n)) = decode(buf).expect("decode") {
            got.push(m);
            buf = &buf[n..];
        }
        assert_eq!(got, all());
        assert!(buf.is_empty());
    }

    #[test]
    fn truncated_and_oversized_frames_are_typed_errors() {
        let f = encode(&all()[1]);
        // An incomplete stream frame waits for more bytes.
        assert_eq!(decode(&f[..f.len() - 1]), Ok(None));
        assert_eq!(decode(&f[..3]), Ok(None));
        // A body cut short (length field rewritten to match) is truncated, never a panic.
        for cut in 1..f.len() - 4 {
            let mut g = f[..4 + cut].to_vec();
            g[..4].copy_from_slice(&(cut as u32).to_le_bytes());
            assert!(decode(&g).is_err(), "cut {cut}");
        }
        assert_eq!(decode_body(&[T_EXITED, 1]), Err(WireError::Truncated));
        // A huge list count is truncation, not an allocation.
        assert_eq!(
            decode_body(&[T_OPEN, 0xff, 0xff, 0xff, 0xff]),
            Err(WireError::Truncated)
        );
        let big = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert_eq!(decode(&big), Err(WireError::Oversized(MAX_FRAME + 1)));
        assert_eq!(decode(&[0, 0, 0, 0]), Err(WireError::Oversized(0)));
        assert_eq!(decode_body(&[99]), Err(WireError::UnknownTag(99)));
        assert_eq!(decode_body(&[T_CLOSE, 0]), Err(WireError::Trailing));
        // The display must be absolute: empty or relative is rejected at the wire boundary.
        for bad in ["", "wayland-1"] {
            let f = encode(&Msg::Open(Open {
                display: bad.into(),
                ..Open::default()
            }));
            assert_eq!(decode(&f), Err(WireError::BadDisplay), "{bad:?}");
        }
    }

    #[test]
    fn hello_verdict_falls_back_on_version_mismatch() {
        let h = |v, b: &str| Msg::Hello {
            protocol_version: v,
            build_id: b.into(),
        };
        assert_eq!(
            judge_hello(&h(PROTOCOL_VERSION, "x"), "x"),
            HelloVerdict::Ok
        );
        assert_eq!(
            judge_hello(&h(PROTOCOL_VERSION, "y"), "x"),
            HelloVerdict::BuildMismatch
        );
        assert_eq!(
            judge_hello(&h(PROTOCOL_VERSION + 1, "x"), "x"),
            HelloVerdict::Fallback
        );
        assert_eq!(judge_hello(&Msg::Close, "x"), HelloVerdict::Fallback);
        // A v1 (pre-session-key) or v2 (pre-display) peer falls back rather than misparsing `open`.
        assert_eq!(judge_hello(&h(1, "x"), "x"), HelloVerdict::Fallback);
        assert_eq!(judge_hello(&h(2, "x"), "x"), HelloVerdict::Fallback);
    }
}

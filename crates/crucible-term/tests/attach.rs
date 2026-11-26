//! Daemon viewer tests (the `Attach` family of the wire) against the TEST-ONLY headless daemon
//! over the raw socket. Each test runs its own `cterm daemon` on private runtime/state/config
//! dirs; the guard SIGKILLs only that daemon. Pictures are judged by feeding a viewer's `Output`
//! bytes to a `Session` of the viewer's size.
#![allow(missing_docs)]

use crucible_term_core::Session;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_cterm");
const KEY_A: &str = "ab12cd3400000000000000000000000a";
const KEY_B: &str = "ab12ef5600000000000000000000000b";

const T_HELLO: u8 = 1;
const T_OPEN: u8 = 2;
const T_OPENED: u8 = 3;
const T_REFUSED: u8 = 4;
const T_EXITED: u8 = 5;
const T_LIST: u8 = 8;
const T_ATTACH: u8 = 9;
const T_INPUT: u8 = 10;
const T_RESIZE: u8 = 11;
const T_DETACH: u8 = 12;
const T_KILL: u8 = 13;
const T_SESSIONS: u8 = 14;
const T_ATTACHED: u8 = 15;
const T_OUTPUT: u8 = 16;

struct Env {
    root: PathBuf,
    daemon: Option<Child>,
}

impl Env {
    /// A private daemon with `extra` env and `config` (config.kdl).
    fn new(name: &str, extra: &[(&str, &str)], config: &str) -> Env {
        let root = std::env::temp_dir().join(format!("cta-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["run", "state", "cfg/crucible-term", "home"] {
            std::fs::create_dir_all(root.join(d)).expect("mkdir");
        }
        std::fs::write(root.join("cfg/crucible-term/config.kdl"), config).unwrap();
        let log = std::fs::File::create(root.join("daemon.log")).unwrap();
        let mut c = Command::new(BIN);
        c.arg("daemon")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("cfg"))
            .env("CTERM_DEBUG_DAEMON_HEADLESS", "1")
            .env("CTERM_DEBUG_HEADLESS_IDENTITY", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        for (k, v) in extra {
            c.env(k, v);
        }
        let daemon = c.spawn().expect("daemon");
        let e = Env {
            root,
            daemon: Some(daemon),
        };
        assert!(
            until(Duration::from_secs(5), || UnixStream::connect(e.socket())
                .is_ok()),
            "daemon listening"
        );
        e
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/cterm/daemon.sock")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn opt(out: &mut Vec<u8>, v: Option<&str>) {
    match v {
        None => out.push(0),
        Some(s) => {
            out.push(1);
            bytes(out, s.as_bytes());
        }
    }
}

fn u32le(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]])
}

enum Rx {
    Frame(u8, Vec<u8>),
    Timeout,
    Eof,
}

/// A raw protocol-3 connection.
struct Conn {
    s: UnixStream,
    buf: Vec<u8>,
}

impl Conn {
    fn new(e: &Env) -> Conn {
        let s = UnixStream::connect(e.socket()).expect("connect");
        let mut c = Conn { s, buf: Vec::new() };
        let mut hello = 3u32.to_le_bytes().to_vec();
        bytes(&mut hello, b"test");
        c.send(T_HELLO, &hello);
        assert!(matches!(
            c.recv(Duration::from_secs(5)),
            Rx::Frame(T_HELLO, _)
        ));
        c
    }

    fn send(&mut self, tag: u8, payload: &[u8]) {
        let mut f = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
        f.push(tag);
        f.extend_from_slice(payload);
        self.s.write_all(&f).expect("send");
    }

    fn recv(&mut self, within: Duration) -> Rx {
        let end = Instant::now() + within;
        loop {
            if self.buf.len() >= 4 {
                let len = u32le(&self.buf, 0) as usize;
                if self.buf.len() >= 4 + len {
                    let f: Vec<u8> = self.buf.drain(..4 + len).collect();
                    return Rx::Frame(f[4], f[5..].to_vec());
                }
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Rx::Timeout;
            }
            self.s.set_read_timeout(Some(left)).unwrap();
            let mut chunk = [0u8; 65536];
            match self.s.read(&mut chunk) {
                Ok(0) => return Rx::Eof,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Rx::Timeout;
                }
                Err(e) if e.kind() == ErrorKind::ConnectionReset => return Rx::Eof,
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    /// The next frame, which must come within 5 s.
    fn next(&mut self) -> (u8, Vec<u8>) {
        match self.recv(Duration::from_secs(5)) {
            Rx::Frame(t, p) => (t, p),
            Rx::Timeout => panic!("no frame within 5 s"),
            Rx::Eof => panic!("daemon hung up"),
        }
    }

    /// A 0.6.0-shaped window `open` of `sh -c script` (granted and keyed when `key` is given).
    fn open(e: &Env, script: &str, key: Option<&str>) -> (Conn, u32) {
        let mut c = Conn::new(e);
        let mut o = Vec::new();
        let argv = ["sh", "-c", script];
        o.extend_from_slice(&(argv.len() as u32).to_le_bytes());
        for a in argv {
            bytes(&mut o, a.as_bytes());
        }
        bytes(&mut o, b"/");
        o.extend_from_slice(&1u32.to_le_bytes());
        bytes(&mut o, b"PATH");
        bytes(&mut o, std::env::var("PATH").unwrap_or_default().as_bytes());
        o.extend_from_slice(&0u32.to_le_bytes());
        opt(&mut o, None);
        opt(&mut o, None);
        opt(&mut o, key.map(|_| "g"));
        opt(&mut o, key);
        bytes(&mut o, e.root.join("run/w").as_os_str().as_encoded_bytes());
        c.send(T_OPEN, &o);
        let (tag, p) = c.next();
        assert_eq!(
            tag,
            T_OPENED,
            "opened, got {tag} {:?}",
            String::from_utf8_lossy(&p)
        );
        (c, u32le(&p, 0))
    }

    fn attach(&mut self, token: &str, cols: u32, rows: u32) {
        let mut p = Vec::new();
        bytes(&mut p, token.as_bytes());
        p.extend_from_slice(&cols.to_le_bytes());
        p.extend_from_slice(&rows.to_le_bytes());
        self.send(T_ATTACH, &p);
    }

    fn input(&mut self, b: &[u8]) {
        let mut p = Vec::new();
        bytes(&mut p, b);
        self.send(T_INPUT, &p);
    }

    fn resize(&mut self, cols: u32, rows: u32) {
        let mut p = cols.to_le_bytes().to_vec();
        p.extend_from_slice(&rows.to_le_bytes());
        self.send(T_RESIZE, &p);
    }

    fn kill(&mut self, token: &str) {
        let mut p = Vec::new();
        bytes(&mut p, token.as_bytes());
        self.send(T_KILL, &p);
    }

    fn refused(&mut self) -> String {
        let (tag, p) = self.next();
        assert_eq!(tag, T_REFUSED, "refused, got {tag}");
        String::from_utf8(p[4..].to_vec()).unwrap()
    }

    fn list(&mut self) -> Vec<Entry> {
        self.send(T_LIST, &[]);
        let (tag, p) = self.next();
        assert_eq!(tag, T_SESSIONS);
        parse_sessions(&p)
    }
}

#[derive(Debug)]
struct Entry {
    id: u64,
    key: Option<String>,
    pid: u32,
    title: String,
    size: (u32, u32),
    has_window: bool,
    viewers: u32,
}

fn parse_sessions(p: &[u8]) -> Vec<Entry> {
    let mut at = 0;
    let take = |at: &mut usize, n: usize| {
        *at += n;
        &p[*at - n..*at]
    };
    let u32at = |at: &mut usize| u32::from_le_bytes(take(at, 4).try_into().unwrap());
    let string = |at: &mut usize| {
        let n = u32::from_le_bytes(take(at, 4).try_into().unwrap()) as usize;
        String::from_utf8(take(at, n).to_vec()).unwrap()
    };
    let n = u32at(&mut at);
    (0..n)
        .map(|_| {
            let id = u64::from_le_bytes(take(&mut at, 8).try_into().unwrap());
            let key = (take(&mut at, 1)[0] != 0).then(|| string(&mut at));
            let pid = u32at(&mut at);
            let title = string(&mut at);
            let _cwd = string(&mut at);
            let size = (u32at(&mut at), u32at(&mut at));
            let has_window = take(&mut at, 1)[0] != 0;
            let viewers = u32at(&mut at);
            Entry {
                id,
                key,
                pid,
                title,
                size,
                has_window,
                viewers,
            }
        })
        .collect()
}

/// A viewer: its connection and the picture its `Output` paints.
struct Viewer {
    c: Conn,
    pic: Session,
    raw: Vec<u8>,
    tags: Vec<u8>,
    code: Option<i32>,
    eof: bool,
}

impl Viewer {
    /// Attach to `token` at `cols` x `rows`; returns the viewer and the `Attached` session size.
    fn attach(e: &Env, token: &str, cols: u32, rows: u32) -> (Viewer, (u32, u32)) {
        let mut c = Conn::new(e);
        c.attach(token, cols, rows);
        let (tag, p) = c.next();
        assert_eq!(
            tag,
            T_ATTACHED,
            "attached, got {tag} {:?}",
            String::from_utf8_lossy(&p)
        );
        let size = (u32le(&p, 8), u32le(&p, 12));
        let v = Viewer {
            c,
            pic: Session::new(cols as usize, rows as usize, 0),
            raw: Vec::new(),
            tags: Vec::new(),
            code: None,
            eof: false,
        };
        (v, size)
    }

    /// Read for `within`, or until `f` holds; true when it held.
    fn read_until(&mut self, within: Duration, f: impl Fn(&Viewer) -> bool) -> bool {
        let end = Instant::now() + within;
        while !f(self) {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || self.eof {
                return false;
            }
            match self.c.recv(left) {
                Rx::Frame(tag, p) => self.take(tag, p),
                Rx::Timeout => return f(self),
                Rx::Eof => self.eof = true,
            }
        }
        true
    }

    fn take(&mut self, tag: u8, p: Vec<u8>) {
        self.tags.push(tag);
        match tag {
            T_OUTPUT => {
                let b = &p[4..];
                self.pic.feed(b);
                self.raw.extend_from_slice(b);
            }
            T_EXITED => self.code = Some(u32le(&p, 0) as i32),
            _ => {}
        }
    }

    fn row(&self, y: usize) -> String {
        self.pic.screen_row(y).text()
    }

    fn shows(&self, needle: &str) -> bool {
        let (_, rows) = self.pic.size();
        (0..rows).any(|y| self.row(y).contains(needle))
    }

    fn rows(&self) -> Vec<String> {
        let (_, rows) = self.pic.size();
        (0..rows).map(|y| self.row(y)).collect()
    }
}

const SEC: Duration = Duration::from_secs(1);

/// A child that echoes nothing (raw), appends each input line to `log`, and on `w`, `t`, `q`
/// draws a 90-column line, sets the title and rings, or exits 7.
fn interactive(log: &std::path::Path) -> String {
    format!(
        "stty -echo; echo READY; while read l; do echo \"$l\" >> {log}; case $l in \
         w) printf '%s\\n' 012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789;; \
         t) printf '\\033]2;hello\\007\\007';; q) exit 7;; esac; done",
        log = log.display()
    )
}

#[test]
fn a_0_6_0_window_open_is_served_and_listed() {
    let e = Env::new("list", &[], "");
    let (_w, pid) = Conn::open(&e, "printf '\\033]2;top\\007'; exec sleep 100", Some(KEY_A));
    let (_w2, pid2) = Conn::open(&e, "exec sleep 100", None);
    let mut c = Conn::new(&e);
    assert!(until(Duration::from_secs(5), || {
        Conn::new(&e).list().iter().any(|s| s.title == "top")
    }));
    let l = c.list();
    assert_eq!(l.len(), 2, "{l:?}");
    assert_eq!((l[0].pid, l[0].key.as_deref()), (pid, Some(KEY_A)));
    assert_eq!((l[1].pid, l[1].key.as_deref()), (pid2, None));
    assert!(
        l.iter()
            .all(|s| s.has_window && s.viewers == 0 && s.size.0 > 0)
    );
    let (_v, _) = Viewer::attach(&e, &l[1].id.to_string(), 30, 5);
    let l = c.list();
    assert_eq!((l[0].viewers, l[1].viewers), (0, 1));
}

#[test]
fn attach_by_id_or_key_prefix_and_refusals_are_stable() {
    let e = Env::new("token", &[], "");
    let (_a, _) = Conn::open(&e, "echo AAA; exec sleep 100", Some(KEY_A));
    let (_b, _) = Conn::open(&e, "echo BBB; exec sleep 100", Some(KEY_B));
    let ids: Vec<u64> = Conn::new(&e).list().iter().map(|s| s.id).collect();
    let (mut v, _) = Viewer::attach(&e, &ids[1].to_string(), 40, 6);
    assert!(v.read_until(5 * SEC, |v| v.shows("BBB")));
    let (mut v, _) = Viewer::attach(&e, "AB12C", 40, 6);
    assert!(v.read_until(5 * SEC, |v| v.shows("AAA")));
    let mut c = Conn::new(&e);
    c.attach("ab12", 40, 6);
    assert_eq!(c.refused(), "ambiguous");
    c.attach("999", 40, 6);
    assert_eq!(c.refused(), "no-session");
    c.attach("1", 0, 6);
    assert_eq!(c.refused(), "bad-size");
    c.attach("1", 40, 1001);
    assert_eq!(c.refused(), "bad-size");
    c.kill("ab12");
    assert_eq!(c.refused(), "ambiguous");
    // An attached viewer cannot attach again.
    v.c.attach("1", 40, 6);
    assert!(v.read_until(5 * SEC, |v| v.tags.contains(&T_REFUSED)));
}

#[test]
fn two_viewers_of_different_sizes_each_see_their_clip_and_both_type() {
    let e = Env::new("two", &[], "");
    let log = e.file("in");
    let (_w, _) = Conn::open(&e, &interactive(&log), None);
    let (mut a, _) = Viewer::attach(&e, "1", 40, 10);
    assert!(a.read_until(5 * SEC, |v| v.shows("READY")));
    let (mut b, size) = Viewer::attach(&e, "1", 100, 30);
    assert_eq!(size, (100, 30), "the attaching viewer sizes the session");
    b.c.input(b"w\n");
    let digits = "0123456789".repeat(9);
    assert!(
        b.read_until(5 * SEC, |v| v.shows(&digits)),
        "{:?}",
        b.rows()
    );
    assert!(
        a.read_until(5 * SEC, |v| v.shows(&digits[..40])),
        "{:?}",
        a.rows()
    );
    assert!(a.rows().iter().all(|r| r.chars().count() <= 40));
    a.c.input(b"from-a\n");
    b.c.input(b"from-b\n");
    assert!(until(5 * SEC, || {
        std::fs::read_to_string(&log)
            .is_ok_and(|s| s.contains("from-a\n") && s.contains("from-b\n"))
    }));
}

#[test]
fn the_size_returns_to_the_window_or_the_last_active_viewer() {
    let e = Env::new("handover", &[("CTERM_DEBUG_HEADLESS_LOSS_MS", "1500")], "");
    let (_w, _) = Conn::open(&e, "exec sleep 100", None);
    let mut c = Conn::new(&e);
    let window = c.list()[0].size;
    // With a window: the owner leaving hands the size back to it.
    let (mut a, s) = Viewer::attach(&e, "1", 50, 12);
    assert_eq!(s, (50, 12));
    a.c.send(T_DETACH, &[]);
    assert!(a.read_until(5 * SEC, |v| v.eof));
    assert_eq!(c.list()[0].size, window);
    // Detached (the fake display is lost): the most recently active remaining viewer.
    assert!(until(5 * SEC, || !c.list()[0].has_window));
    let (mut a, _) = Viewer::attach(&e, "1", 50, 12);
    let (b, s) = Viewer::attach(&e, "1", 70, 20);
    assert_eq!(s, (70, 20));
    // A resize by a non-owner changes nothing; the owner's does.
    a.c.resize(60, 15);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(c.list()[0].size, (70, 20));
    // B hangs up (no detach): A's remembered size applies, and the session lives on.
    drop(b);
    assert!(until(5 * SEC, || c.list()[0].size == (60, 15)));
    a.c.resize(61, 16);
    assert!(until(5 * SEC, || c.list()[0].size == (61, 16)));
    assert_eq!(c.list()[0].viewers, 1);
}

#[test]
fn a_stalled_viewer_skips_frames_without_stalling_the_other() {
    let e = Env::new("stall", &[], "");
    let (_w, _) = Conn::open(
        &e,
        "read x; base64 /dev/urandom | head -n 1500000; echo DONE; exec sleep 100",
        None,
    );
    let (mut slow, _) = Viewer::attach(&e, "1", 200, 50);
    let (mut fast, _) = Viewer::attach(&e, "1", 200, 50);
    rustix::net::sockopt::set_socket_recv_buffer_size(&slow.c.s, 4096).unwrap();
    fast.c.input(b"go\n");
    // The fast viewer keeps receiving frames while the slow one reads nothing.
    let t = Instant::now();
    let mut gaps = Duration::ZERO;
    let mut frames = 0;
    let mut last = Instant::now();
    while !fast.shows("DONE") && t.elapsed() < Duration::from_secs(60) {
        match fast.c.recv(Duration::from_secs(5)) {
            Rx::Frame(tag, p) => {
                gaps = gaps.max(last.elapsed());
                last = Instant::now();
                frames += 1;
                fast.take(tag, p);
            }
            _ => panic!("the fast viewer stopped receiving"),
        }
    }
    assert!(fast.shows("DONE"), "{:?}", fast.rows());
    assert!(frames > 10, "{frames} frames");
    assert!(gaps < Duration::from_secs(1), "longest gap {gaps:?}");
    // Once it reads, the slow viewer catches up to the same picture.
    assert!(slow.read_until(10 * SEC, |v| v.shows("DONE")));
    assert!(slow.read_until(SEC / 2, |_| false) || !slow.eof);
    assert_eq!(slow.rows(), fast.rows());
    // It skipped frames: far fewer bytes than the viewer that kept reading.
    assert!(
        slow.raw.len() * 2 < fast.raw.len(),
        "slow {} fast {}",
        slow.raw.len(),
        fast.raw.len()
    );
    assert!(!e.log().contains("undrained"), "{}", e.log());
}

#[test]
fn a_viewer_undrained_past_the_limit_is_dropped_and_the_session_lives() {
    let e = Env::new("drop", &[("CTERM_DEBUG_VIEWER_STALL_MS", "300")], "");
    let (_w, _) = Conn::open(&e, "read x; exec base64 /dev/urandom", None);
    let (mut slow, _) = Viewer::attach(&e, "1", 200, 50);
    rustix::net::sockopt::set_socket_recv_buffer_size(&slow.c.s, 4096).unwrap();
    slow.c.input(b"go\n");
    assert!(
        until(30 * SEC, || e.log().contains("undrained too long; dropped")),
        "{}",
        e.log()
    );
    assert!(
        slow.read_until(10 * SEC, |v| v.eof),
        "dropped: EOF after the queued bytes"
    );
    let l = Conn::new(&e).list();
    assert_eq!(
        (l.len(), l[0].viewers),
        (1, 0),
        "a dropped viewer closes nothing"
    );
}

#[test]
fn kill_answers_once_and_every_viewer_hears_the_exit() {
    let e = Env::new("kill", &[], "");
    let (mut w, _) = Conn::open(&e, "trap 'exit 3' HUP; while :; do sleep 0.05; done", None);
    let (mut v, _) = Viewer::attach(&e, "1", 40, 8);
    let mut k = Conn::new(&e);
    k.kill("1");
    let (tag, p) = k.next();
    assert_eq!((tag, u32le(&p, 0)), (T_EXITED, 3));
    assert!(matches!(k.recv(2 * SEC), Rx::Eof), "exactly one reply");
    assert!(v.read_until(5 * SEC, |v| v.eof));
    assert_eq!(v.code, Some(3));
    assert_eq!(v.tags.iter().filter(|&&t| t == T_EXITED).count(), 1);
    assert!(
        v.raw.ends_with(b"\x1b[?1049l"),
        "leave sequence before exited"
    );
    assert_eq!(w.next().0, T_EXITED, "the window hears it too");
    // A viewer that kills its own session gets one `Exited`, not two.
    let (_w2, _) = Conn::open(&e, "trap 'exit 4' HUP; while :; do sleep 0.05; done", None);
    let (mut v, _) = Viewer::attach(&e, "2", 40, 8);
    v.c.kill("2");
    assert!(v.read_until(5 * SEC, |v| v.eof));
    assert_eq!(v.tags.iter().filter(|&&t| t == T_EXITED).count(), 1);
    assert_eq!(v.code, Some(4));
}

#[test]
fn a_child_exit_reaches_every_viewer() {
    let e = Env::new("exit", &[], "");
    let (mut w, _) = Conn::open(&e, &interactive(&e.file("in")), None);
    let (mut a, _) = Viewer::attach(&e, "1", 40, 8);
    let (mut b, _) = Viewer::attach(&e, "1", 50, 9);
    assert!(a.read_until(5 * SEC, |v| v.shows("READY")));
    a.c.input(b"q\n");
    for v in [&mut a, &mut b] {
        assert!(v.read_until(5 * SEC, |v| v.eof));
        assert_eq!(v.code, Some(7));
    }
    let (tag, p) = w.next();
    assert_eq!((tag, u32le(&p, 0)), (T_EXITED, 7));
}

#[test]
fn titles_and_bells_are_forwarded() {
    let e = Env::new("title", &[], "");
    let (_w, _) = Conn::open(&e, &interactive(&e.file("in")), None);
    let (mut a, _) = Viewer::attach(&e, "1", 40, 8);
    assert!(a.read_until(5 * SEC, |v| v.shows("READY")));
    a.c.input(b"t\n");
    let has = |v: &Viewer, n: &[u8]| v.raw.windows(n.len()).any(|w| w == n);
    assert!(a.read_until(5 * SEC, |v| has(v, b"\x1b]2;hello\x1b\\")
        && v.raw.contains(&7)));
    // A later viewer gets the current title right after enter.
    let (mut b, _) = Viewer::attach(&e, "1", 40, 8);
    assert!(b.read_until(5 * SEC, |v| has(v, b"\x1b]2;hello\x1b\\")));
}

#[test]
fn an_idle_attached_session_sends_nothing() {
    let e = Env::new("idle", &[], "");
    let (_w, _) = Conn::open(&e, "echo IDLE; exec sleep 100", None);
    let (mut a, _) = Viewer::attach(&e, "1", 40, 8);
    assert!(a.read_until(5 * SEC, |v| v.shows("IDLE")));
    // Let the first frames settle, then expect silence for a second.
    a.read_until(SEC / 2, |_| false);
    let n = a.raw.len();
    assert!(!a.read_until(SEC, |v| v.raw.len() != n), "idle bytes sent");
}

#[test]
fn an_attach_cancels_the_detached_clock_and_the_last_detach_rearms_it() {
    let e = Env::new(
        "clock",
        &[
            ("CTERM_DEBUG_HEADLESS_LOSS_MS", "200"),
            ("CTERM_DEBUG_HEADLESS_RECONNECT_MS", "100"),
        ],
        "detached-timeout 1\n",
    );
    let (w, _) = Conn::open(&e, "exec sleep 100", None);
    let (mut a, _) = Viewer::attach(&e, "1", 40, 8);
    drop(w);
    assert!(
        until(5 * SEC, || e.log().contains("daemon: reconnected")),
        "{}",
        e.log()
    );
    // The reconnect would arm a 1 s clock, but a viewer is attached.
    std::thread::sleep(Duration::from_millis(1600));
    assert!(!e.log().contains("unclaimed"), "{}", e.log());
    assert_eq!(Conn::new(&e).list().len(), 1);
    a.c.send(T_DETACH, &[]);
    assert!(a.read_until(5 * SEC, |v| v.eof));
    let t = Instant::now();
    assert!(
        until(5 * SEC, || e.log().contains("unclaimed after 1s")),
        "{}",
        e.log()
    );
    assert!(
        t.elapsed() >= Duration::from_millis(800),
        "re-armed at detach"
    );
}

#[test]
fn a_viewer_hang_up_closes_nothing_and_input_without_attach_is_ignored() {
    let e = Env::new("hangup", &[], "");
    let log = e.file("in");
    let (_w, _) = Conn::open(&e, &interactive(&log), None);
    let (mut a, _) = Viewer::attach(&e, "1", 40, 8);
    assert!(a.read_until(5 * SEC, |v| v.shows("READY")));
    let mut stray = Conn::new(&e);
    stray.input(b"stray\n");
    drop(a);
    std::thread::sleep(Duration::from_millis(2500));
    let l = Conn::new(&e).list();
    assert_eq!((l.len(), l[0].viewers), (1, 0));
    let (mut b, _) = Viewer::attach(&e, "1", 40, 8);
    b.c.input(b"later\n");
    assert!(until(5 * SEC, || std::fs::read_to_string(&log)
        .is_ok_and(|s| s.contains("later"))));
    assert!(!std::fs::read_to_string(&log).unwrap().contains("stray"));
}

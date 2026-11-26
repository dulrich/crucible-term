//! `cterm ls` / `attach` / `kill` end to end: the real binary on a PTY against the TEST-ONLY
//! headless daemon on private runtime/state/config dirs (the guard SIGKILLs only that daemon).
//! The viewer's picture is judged by feeding the PTY's output to a `Session`.
#![allow(missing_docs)]

use crucible_term_core::Session;
use crucible_term_core::pty::{Pty, SpawnSpec, exit_code};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_cterm");
const LEAVE_ALL: &[u8] = b"\x1b[0m\x1b[r\x1b[0 q\x1b[?25h\x1b[?1l\x1b>\x1b[?2004l\x1b[?1004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\x1b[?1049l";

struct Env {
    root: PathBuf,
    daemon: Option<Child>,
    /// Window connections kept open so their sessions stay windowed.
    windows: Vec<UnixStream>,
}

impl Env {
    /// Private dirs; with `daemon`, a headless daemon listening on them.
    fn new(name: &str, daemon: bool) -> Env {
        let root = std::env::temp_dir().join(format!("ctc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["run", "state", "cfg/crucible-term", "home", "bin"] {
            std::fs::create_dir_all(root.join(d)).expect("mkdir");
        }
        let mut e = Env {
            root,
            daemon: None,
            windows: Vec::new(),
        };
        if daemon {
            let log = std::fs::File::create(e.root.join("daemon.log")).unwrap();
            let mut c = Command::new(BIN);
            c.arg("daemon")
                .env_clear()
                .envs(e.vars())
                .env("CTERM_DEBUG_DAEMON_HEADLESS", "1")
                .env("CTERM_DEBUG_HEADLESS_IDENTITY", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log);
            e.daemon = Some(c.spawn().expect("daemon"));
            assert!(
                until(Duration::from_secs(5), || UnixStream::connect(e.socket())
                    .is_ok()),
                "daemon listening"
            );
        }
        e
    }

    fn vars(&self) -> Vec<(OsString, OsString)> {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        vec![
            ("PATH".into(), path.into()),
            ("HOME".into(), self.root.join("home").into()),
            ("XDG_RUNTIME_DIR".into(), self.root.join("run").into()),
            ("XDG_STATE_HOME".into(), self.root.join("state").into()),
            ("XDG_CONFIG_HOME".into(), self.root.join("cfg").into()),
            ("SSH_LOG".into(), self.root.join("ssh.log").into()),
        ]
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/cterm/daemon.sock")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// `cterm ARGS` without a terminal.
    fn cterm(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env_clear()
            .envs(self.vars())
            .stdin(Stdio::null())
            .output()
            .expect("cterm")
    }

    /// A windowed session running `sh -c script`; returns its id.
    fn session(&mut self, script: &str) -> u64 {
        let mut s = UnixStream::connect(self.socket()).expect("connect");
        hello(&mut s);
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
        o.extend_from_slice(&[0, 0, 0, 0]);
        bytes(
            &mut o,
            self.root.join("run/w").as_os_str().as_encoded_bytes(),
        );
        send(&mut s, 2, &o);
        let (tag, p) = recv(&mut s);
        assert_eq!(
            tag,
            3,
            "opened, got {tag} {:?}",
            String::from_utf8_lossy(&p)
        );
        let pid = u32::from_le_bytes(p[..4].try_into().unwrap());
        self.windows.push(s);
        let mut id = None;
        assert!(until(Duration::from_secs(5), || {
            id = self
                .ls()
                .into_iter()
                .find(|l| l[2] == pid.to_string())
                .map(|l| l[0].parse().unwrap());
            id.is_some()
        }));
        id.unwrap()
    }

    /// `cterm ls`, split into words per line.
    fn ls(&self) -> Vec<Vec<String>> {
        let o = self.cterm(&["ls"]);
        assert!(
            o.status.success(),
            "ls: {:?}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .map(|l| l.split_whitespace().map(String::from).collect())
            .collect()
    }

    /// The size column of session `id`.
    fn size_of(&self, id: u64) -> Option<String> {
        self.ls()
            .into_iter()
            .find(|l| l[0] == id.to_string())
            .map(|l| l[3].clone())
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

fn send(s: &mut UnixStream, tag: u8, payload: &[u8]) {
    let mut f = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
    f.push(tag);
    f.extend_from_slice(payload);
    s.write_all(&f).expect("send");
}

fn recv(s: &mut UnixStream) -> (u8, Vec<u8>) {
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut len = [0u8; 4];
    s.read_exact(&mut len).expect("frame length");
    let mut f = vec![0u8; u32::from_le_bytes(len) as usize];
    s.read_exact(&mut f).expect("frame");
    (f[0], f[1..].to_vec())
}

fn hello(s: &mut UnixStream) {
    let mut h = 3u32.to_le_bytes().to_vec();
    bytes(&mut h, b"test");
    send(s, 1, &h);
    assert_eq!(recv(s).0, 1, "hello");
}

/// The termios fields that raw mode changes.
fn modes(f: &std::fs::File) -> (u32, u32, u32, u32) {
    let t = rustix::termios::tcgetattr(f).expect("tcgetattr");
    (
        t.input_modes.bits() as u32,
        t.output_modes.bits() as u32,
        t.local_modes.bits() as u32,
        t.control_modes.bits() as u32,
    )
}

/// `cterm ARGS` on a PTY of 80x24, with the picture its output paints.
struct Term {
    pty: Pty,
    pic: Session,
    raw: Vec<u8>,
    /// The PTY's termios before cterm started.
    before: (u32, u32, u32, u32),
    code: Option<i32>,
}

impl Term {
    fn spawn(e: &Env, args: &[&str]) -> Term {
        // A short sleep before the exec lets the PTY's starting termios be read first.
        let mut argv: Vec<OsString> = ["sh", "-c", "sleep 0.3; exec \"$0\" \"$@\"", BIN]
            .iter()
            .map(OsString::from)
            .collect();
        argv.extend(args.iter().map(OsString::from));
        let pty = Pty::spawn(&SpawnSpec {
            argv,
            cwd: "/".into(),
            env: e.vars(),
            size: (80, 24),
        })
        .expect("pty");
        let before = modes(pty.master());
        Term {
            pty,
            pic: Session::new(80, 24, 0),
            raw: Vec::new(),
            before,
            code: None,
        }
    }

    /// Read for `within`, or until `f` holds; true when it held.
    fn pump(&mut self, within: Duration, f: impl Fn(&Term) -> bool) -> bool {
        use rustix::event::{PollFd, PollFlags, poll};
        let end = Instant::now() + within;
        while !f(self) {
            if Instant::now() >= end {
                return false;
            }
            let ts = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 20_000_000,
            };
            let ready = {
                let mut fds = [PollFd::new(self.pty.master(), PollFlags::IN)];
                matches!(poll(&mut fds, Some(&ts)), Ok(n) if n > 0)
            };
            if ready {
                let mut b = [0u8; 65536];
                match self.pty.read(&mut b) {
                    Ok(n) if n > 0 => {
                        self.pic.feed(&b[..n]);
                        self.raw.extend_from_slice(&b[..n]);
                    }
                    _ => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            if self.code.is_none()
                && let Ok(Some(s)) = self.pty.try_wait()
            {
                self.code = Some(exit_code(s));
            }
        }
        true
    }

    fn shows(&self, needle: &str) -> bool {
        (0..24).any(|y| self.pic.screen_row(y).text().contains(needle))
    }

    fn wait_shows(&mut self, needle: &str) {
        assert!(
            self.pump(Duration::from_secs(5), |t| t.shows(needle)),
            "{needle:?} never shown: {:?}",
            String::from_utf8_lossy(&self.raw)
        );
    }

    fn type_(&mut self, b: &[u8]) {
        self.pty.write_all(b).expect("type");
    }

    /// The exit code, within 5 s (the output read meanwhile).
    fn exit(&mut self) -> i32 {
        assert!(
            self.pump(Duration::from_secs(5), |t| t.code.is_some()),
            "cterm still running: {:?}",
            String::from_utf8_lossy(&self.raw)
        );
        // Whatever it wrote last.
        self.pump(Duration::from_millis(100), |_| false);
        self.code.unwrap()
    }

    fn restored(&self) -> bool {
        modes(self.pty.master()) == self.before
    }
}

fn read_file(p: &std::path::Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// A child that echoes nothing, appends each input line to `log`, and exits 7 on `q`.
fn interactive(log: &std::path::Path) -> String {
    format!(
        "stty -echo; echo READY; while read l; do echo \"$l\" >> {log}; case $l in q) exit 7;; esac; done",
        log = log.display()
    )
}

#[test]
fn attach_shows_types_resizes_and_detaches_by_prefix() {
    let mut e = Env::new("basic", true);
    let log = e.file("in.log");
    let id = e.session(&interactive(&log));
    let mut t = Term::spawn(&e, &["attach", &id.to_string()]);
    t.wait_shows("READY");
    assert_ne!(modes(t.pty.master()), t.before, "raw mode while attached");
    t.type_(b"hello\r");
    assert!(until(Duration::from_secs(5), || read_file(&log).contains("hello")));
    t.pty.resize((100, 30)).expect("resize");
    assert!(
        until(Duration::from_secs(5), || e.size_of(id).as_deref()
            == Some("100x30")),
        "{:?}",
        e.size_of(id)
    );
    t.type_(b"\x1cd");
    assert_eq!(t.exit(), 0, "{:?}", String::from_utf8_lossy(&t.raw));
    assert!(t.restored(), "termios restored");
    assert!(
        t.raw.ends_with(b"\x1b[?1049l"),
        "the daemon's leave ran last"
    );
    assert!(!t.raw.ends_with(LEAVE_ALL), "no unclean reset on a detach");
    // The session lives on, detached from this viewer.
    assert!(e.size_of(id).is_some());
}

#[test]
fn the_kitty_prefix_detaches_across_reads() {
    let mut e = Env::new("kitty", true);
    let id = e.session(&interactive(&e.file("in.log")));
    let mut t = Term::spawn(&e, &["attach", &id.to_string()]);
    t.wait_shows("READY");
    for part in [&b"\x1b[9"[..], b"2;5", b"u\x1b[92;5:3u", b"\x1b[100", b"u"] {
        t.type_(part);
        std::thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(t.exit(), 0);
    assert!(t.restored());
}

#[test]
fn prefix_twice_delivers_one_0x1c() {
    let mut e = Env::new("twice", true);
    let log = e.file("od.log");
    let id = e.session(&format!(
        "stty raw -echo; echo READY; head -c2 | od -An -tx1 > {}; exec sleep 100",
        log.display()
    ));
    let mut t = Term::spawn(&e, &["attach", &id.to_string()]);
    t.wait_shows("READY");
    t.type_(b"\x1c\x1cx");
    assert!(
        until(Duration::from_secs(5), || read_file(&log)
            .split_whitespace()
            .count()
            == 2),
        "{:?}",
        read_file(&log)
    );
    assert_eq!(
        read_file(&log).split_whitespace().collect::<Vec<_>>(),
        ["1c", "78"]
    );
    t.type_(b"\x1cd");
    assert_eq!(t.exit(), 0);
}

#[test]
fn a_child_exit_relays_its_code() {
    let mut e = Env::new("exit", true);
    let id = e.session(&interactive(&e.file("in.log")));
    let mut t = Term::spawn(&e, &["attach", &id.to_string()]);
    t.wait_shows("READY");
    t.type_(b"q\r");
    assert_eq!(t.exit(), 7);
    assert!(t.restored());
}

#[test]
fn ls_lists_and_kill_ends_or_refuses() {
    let mut e = Env::new("kill", true);
    let id = e.session("printf '\\033]2;top\\007'; exec sleep 100");
    assert!(until(Duration::from_secs(5), || e
        .ls()
        .iter()
        .any(|l| l.contains(&"top".to_string()))));
    let l = e.ls();
    assert_eq!(l.len(), 1, "{l:?}");
    assert_eq!(
        (l[0][0].as_str(), l[0][1].as_str(), l[0][4].as_str()),
        (id.to_string().as_str(), "-", "window")
    );
    assert_eq!(l[0][5], "0");
    let o = e.cterm(&["kill", &id.to_string()]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{:?}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(until(Duration::from_secs(5), || e.ls().is_empty()));
    assert!(e.cterm(&["ls"]).stdout.is_empty());
    let o = e.cterm(&["kill", "999"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("no session"),
        "{o:?}"
    );
}

#[test]
fn attach_needs_a_terminal_and_reports_a_refusal() {
    let mut e = Env::new("refuse", true);
    let id = e.session("exec sleep 100");
    let o = e.cterm(&["attach", &id.to_string()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("terminal"),
        "{o:?}"
    );
    let mut t = Term::spawn(&e, &["attach", "999"]);
    assert_eq!(t.exit(), 1);
    assert!(t.restored());
    assert!(String::from_utf8_lossy(&t.raw).contains("no session"));
}

#[test]
fn host_forms_run_ssh_with_exact_argv() {
    let e = Env::new("ssh", false);
    let stub = e.file("bin/ssh");
    std::fs::write(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SSH_LOG\"\nexit 5\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let ssh_log = e.file("ssh.log");
    for (args, want) in [
        (&["ls", "h1"][..], "--\nh1\ncterm\nls\n"),
        (&["kill", "u@h.x:ab12"], "--\nu@h.x\ncterm\nkill\nab12\n"),
        (&["attach", "h-2:7"], "-t\n--\nh-2\ncterm\nattach\n7\n"),
    ] {
        let _ = std::fs::remove_file(&ssh_log);
        let o = e.cterm(args);
        assert_eq!(o.status.code(), Some(5), "{args:?} {o:?}");
        assert_eq!(read_file(&ssh_log), want, "{args:?}");
    }
    let _ = std::fs::remove_file(&ssh_log);
    for args in [
        &["attach", "-oProxyCommand=x:1"][..],
        &["ls", "-v"],
        &["kill", "h:1;id"],
    ] {
        assert_eq!(e.cterm(args).status.code(), Some(2), "{args:?}");
    }
    assert!(!ssh_log.exists(), "ssh never ran for a rejected argument");
    // The attach form resets the local terminal after ssh, however it ended.
    let mut t = Term::spawn(&e, &["attach", "h:1"]);
    assert_eq!(t.exit(), 5);
    assert!(
        t.raw.ends_with(LEAVE_ALL),
        "{:?}",
        String::from_utf8_lossy(&t.raw)
    );
    assert!(t.restored());
}

#[test]
fn daemon_loss_restores_the_terminal_and_exits_1() {
    let mut e = Env::new("loss", true);
    let id = e.session(&interactive(&e.file("in.log")));
    let mut t = Term::spawn(&e, &["attach", &id.to_string()]);
    t.wait_shows("READY");
    let mut d = e.daemon.take().unwrap();
    d.kill().unwrap();
    d.wait().unwrap();
    assert_eq!(t.exit(), 1);
    assert!(t.restored(), "termios restored");
    assert!(
        t.raw.windows(LEAVE_ALL.len()).any(|w| w == LEAVE_ALL)
            && String::from_utf8_lossy(&t.raw).ends_with("daemon connection lost\r\n"),
        "{:?}",
        String::from_utf8_lossy(&t.raw)
    );
}

#[test]
fn sigterm_restores_the_terminal() {
    let mut e = Env::new("term", true);
    let id = e.session(&interactive(&e.file("in.log")));
    let mut t = Term::spawn(&e, &["attach", &id.to_string()]);
    t.wait_shows("READY");
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(t.pty.pid() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert_eq!(t.exit(), 128 + 15);
    assert!(t.restored());
    assert!(t.raw.ends_with(LEAVE_ALL));
}

#[test]
fn an_old_daemon_or_none_is_named() {
    let e = Env::new("old", false);
    // No socket: one message, exit 1, and nothing spawned.
    let o = e.cterm(&["ls"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("cannot reach the daemon"),
        "{o:?}"
    );
    assert!(!e.socket().exists());
    // A daemon that answers hello, then closes on the unknown tag.
    std::fs::create_dir_all(e.file("run/cterm")).unwrap();
    let l = UnixListener::bind(e.socket()).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let _ = recv(&mut s);
        let mut h = 3u32.to_le_bytes().to_vec();
        bytes(&mut h, b"oldbuild");
        send(&mut s, 1, &h);
        let mut b = [0u8; 64];
        let _ = s.read(&mut b);
    });
    let o = e.cterm(&["ls"]);
    server.join().unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("predates attach") && err.contains("oldbuild"),
        "{err}"
    );
}

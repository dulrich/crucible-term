//! Daemon integration tests in the TEST-ONLY headless mode (`CTERM_DEBUG_DAEMON_HEADLESS=1`):
//! each test has its own runtime/state/config dirs, and its guard SIGKILLs only the daemon it
//! started (pid from `SO_PEERCRED` on the private socket), even on failure.
#![allow(missing_docs)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_cterm");

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = std::env::temp_dir().join(format!("ctd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["run", "state", "cfg/crucible-term", "home"] {
            std::fs::create_dir_all(root.join(d)).expect("mkdir");
        }
        Env { root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/cterm/w.sock")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("WAYLAND_DISPLAY", "w")
            .env("CTERM_DEBUG_DAEMON_HEADLESS", "1")
            .args(["-o", "mode=daemon"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        c
    }

    fn sh(&self, script: &str) -> Command {
        self.cmd(&["-e", "sh", "-c", script])
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("state/crucible-term/daemon.log"))
            .unwrap_or_default()
    }

    /// The daemon's pid, from `SO_PEERCRED` on a probe connection.
    fn daemon_pid(&self) -> Option<i32> {
        let s = UnixStream::connect(self.socket()).ok()?;
        let c = rustix::net::sockopt::socket_peercred(&s).ok()?;
        Some(c.pid.as_raw_nonzero().get())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid().and_then(rustix::process::Pid::from_raw) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn status(mut c: Command) -> (i32, String) {
    let out = c.output().expect("run cterm");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn wait_for(p: &Path, within: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if p.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

#[test]
fn exit_codes_are_relayed_and_the_daemon_stays() {
    let e = Env::new("relay");
    assert_eq!(status(e.sh("exit 0")), (0, String::new()));
    let pid = e.daemon_pid().expect("daemon listening");
    assert_eq!(status(e.sh("exit 3")).0, 3);
    assert_eq!(status(e.sh("kill -TERM $$")).0, 143);
    assert_eq!(e.daemon_pid(), Some(pid), "one resident daemon");
    assert!(e.log().contains("headless test mode"));
}

#[test]
fn two_clients_at_once_start_one_daemon() {
    let e = Env::new("race");
    let a: Child = e.sh("sleep 0.3").spawn().expect("a");
    let b: Child = e.sh("sleep 0.3").spawn().expect("b");
    for c in [a, b] {
        let out = c.wait_with_output().expect("wait");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(e.log().matches("listening on").count(), 1, "{}", e.log());
    assert!(e.daemon_pid().is_some());
}

#[test]
fn client_sigkill_hangs_up_the_child() {
    let e = Env::new("kill");
    let ready = e.root.join("ready");
    let hup = e.root.join("hup");
    let script = format!(
        "trap 'echo hup > {}; exit 0' HUP; echo $$ > {}; while :; do sleep 0.05; done",
        hup.display(),
        ready.display()
    );
    let mut c = e.sh(&script).spawn().expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)), "child started");
    c.kill().expect("SIGKILL client");
    let _ = c.wait();
    assert!(
        wait_for(&hup, Duration::from_secs(6)),
        "child got SIGHUP (after the orphan grace)"
    );
    let child: i32 = std::fs::read_to_string(&ready)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let t = Instant::now();
    while alive(child) && t.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(child), "child reaped");
}

#[test]
fn client_sighup_hangs_up_without_close_and_the_session_closes_after_the_grace() {
    let e = Env::new("hup");
    let ready = e.root.join("ready");
    let hup = e.root.join("hup");
    let script = format!(
        "trap 'echo hup > {}; exit 0' HUP; echo $$ > {}; while :; do sleep 0.05; done",
        hup.display(),
        ready.display()
    );
    let c = e.sh(&script).spawn().expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)));
    let pid = rustix::process::Pid::from_raw(c.id() as i32).unwrap();
    let t = Instant::now();
    rustix::process::kill_process(pid, rustix::process::Signal::HUP).unwrap();
    let out = c.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(129),
        "exits at once with 128 + SIGHUP"
    );
    assert!(t.elapsed() < Duration::from_secs(1), "no close round trip");
    assert!(
        wait_for(&hup, Duration::from_secs(6)),
        "child hung up after the orphan grace"
    );
    assert!(
        t.elapsed() >= Duration::from_millis(1900),
        "not before the grace"
    );
}

#[test]
fn client_sigterm_sends_close_and_exits_with_the_grace_code() {
    let e = Env::new("term");
    let ready = e.root.join("ready");
    // The child ignores SIGHUP, so the grace runs out: SIGKILL, recorded as 128 + 1.
    let script = format!(
        "trap '' HUP; echo $$ > {}; while :; do sleep 0.05; done",
        ready.display()
    );
    let c = e.sh(&script).spawn().expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)));
    let pid = rustix::process::Pid::from_raw(c.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let out = c.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(129));
}

/// One frame: u32 LE length, tag, payload.
fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = ((payload.len() + 1) as u32).to_le_bytes().to_vec();
    f.push(tag);
    f.extend_from_slice(payload);
    f
}

fn bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

#[test]
fn the_grant_never_reaches_the_daemon_or_the_child() {
    let e = Env::new("grant");
    // A granted spawn starts the daemon, is refused `no-identity` and falls back to standalone
    // (which fails here: no compositor). The daemon was spawned without the grant.
    let mut c = e.sh("exit 0");
    c.env("CRUCIBLE_WINDOW_GRANT", "secret-grant");
    let (_, err) = status(c);
    assert!(err.contains("no window identity"), "{err}");
    let pid = e.daemon_pid().expect("daemon started");
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(!String::from_utf8_lossy(&environ).contains("CRUCIBLE_WINDOW_GRANT"));

    // A raw `open` that smuggles the grant in its env (grant field absent): the daemon strips it.
    let dump = e.root.join("env");
    let mut s = UnixStream::connect(e.socket()).unwrap();
    let mut hello = 2u32.to_le_bytes().to_vec();
    bytes(&mut hello, b"test");
    s.write_all(&frame(1, &hello)).unwrap();
    let mut open = Vec::new();
    let argv = [
        "sh".to_string(),
        "-c".into(),
        format!("env > {}", dump.display()),
    ];
    open.extend_from_slice(&(argv.len() as u32).to_le_bytes());
    for a in &argv {
        bytes(&mut open, a.as_bytes());
    }
    bytes(&mut open, b"/");
    open.extend_from_slice(&2u32.to_le_bytes());
    bytes(&mut open, b"CRUCIBLE_WINDOW_GRANT");
    bytes(&mut open, b"smuggled");
    bytes(&mut open, b"PATH");
    bytes(
        &mut open,
        std::env::var("PATH").unwrap_or_default().as_bytes(),
    );
    open.extend_from_slice(&0u32.to_le_bytes());
    open.extend_from_slice(&[0, 0, 0, 0]);
    s.write_all(&frame(2, &open)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // hello, opened, exited: read until the exited frame (tag 5) arrives.
    let mut buf = Vec::new();
    let mut tags = Vec::new();
    while !tags.contains(&5u8) {
        let mut chunk = [0u8; 256];
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "daemon hung up; tags {tags:?}");
        buf.extend_from_slice(&chunk[..n]);
        while buf.len() >= 5 {
            let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            if buf.len() < 4 + len {
                break;
            }
            tags.push(buf[4]);
            buf.drain(..4 + len);
        }
    }
    assert_eq!(tags, [1, 3, 5]);
    let env = std::fs::read_to_string(&dump).unwrap();
    assert!(env.contains("PATH="));
    assert!(!env.contains("CRUCIBLE_WINDOW_GRANT"), "{env}");
}

#[test]
fn daemon_linger_exits_when_idle() {
    let e = Env::new("linger");
    std::fs::write(
        e.root.join("cfg/crucible-term/config.kdl"),
        "daemon-linger 1\n",
    )
    .unwrap();
    assert_eq!(status(e.sh("exit 0")).0, 0);
    let t = Instant::now();
    while e.socket().exists() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!e.socket().exists(), "daemon exited after linger");
    assert!(e.log().contains("idle for 1s"));
}

#[test]
fn daemon_help_is_usage_on_stdout() {
    let out = Command::new(BIN)
        .args(["daemon", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("usage: cterm daemon"));
}

/// The fake Wayland connection is lost 300 ms after the daemon starts: a client killed while
/// its session is detached does not close it (the child finishes and leaves its marker), and a
/// child that exits while detached is relayed to its connected client.
#[test]
fn a_detached_session_outlives_its_client_and_exits_are_relayed() {
    let e = Env::new("detach");
    let marker = e.root.join("marker");
    let mut c = e.sh(&format!("sleep 1.5; touch {}", marker.display()));
    c.env("CTERM_DEBUG_HEADLESS_LOSS_MS", "300");
    let mut client = c.spawn().expect("client");
    std::thread::sleep(Duration::from_millis(800));
    assert!(e.log().contains("session(s) detached"), "{}", e.log());
    client.kill().expect("kill client");
    let _ = client.wait();
    assert!(
        wait_for(&marker, Duration::from_secs(5)),
        "the detached session was closed on client EOF"
    );
    // Still detached (no reconnect configured): this open is refused and falls back.
    let mut late = e.sh("exit 0");
    late.env("CTERM_DEBUG_HEADLESS_LOSS_MS", "300");
    let (_, err) = status(late);
    assert!(err.contains("no Wayland connection"), "{err}");
}

/// A child exiting while detached is relayed as `exited` to its client.
#[test]
fn an_exit_while_detached_is_relayed() {
    let e = Env::new("detach-exit");
    let mut c = e.sh("sleep 0.8; exit 5");
    c.env("CTERM_DEBUG_HEADLESS_LOSS_MS", "200");
    assert_eq!(status(c).0, 5);
    assert!(e.log().contains("session(s) detached"), "{}", e.log());
}

/// The reconnect timer brings the (fake) display back and later opens are served again.
#[test]
fn the_daemon_reconnects_after_a_loss() {
    let e = Env::new("reconnect");
    let mut c = e.sh("sleep 0.5; exit 0");
    c.env("CTERM_DEBUG_HEADLESS_LOSS_MS", "100")
        .env("CTERM_DEBUG_HEADLESS_RECONNECT_MS", "200");
    assert_eq!(status(c).0, 0);
    let t = Instant::now();
    while !e.log().contains("daemon: reconnected") && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(e.log().contains("daemon: reconnected"), "{}", e.log());
    assert_eq!(status(e.sh("exit 4")).0, 4);
}

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn opt(out: &mut Vec<u8>, v: Option<&str>) {
    match v {
        None => out.push(0),
        Some(s) => {
            out.push(1);
            bytes(out, s.as_bytes());
        }
    }
}

/// A raw v2 client: `hello`, then `open` of `sh -c script` with `grant` and `key`.
struct Raw {
    s: UnixStream,
    buf: Vec<u8>,
}

impl Raw {
    fn open(e: &Env, script: &str, grant: Option<&str>, key: Option<&str>) -> Raw {
        let mut s = UnixStream::connect(e.socket()).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut hello = 2u32.to_le_bytes().to_vec();
        bytes(&mut hello, b"test");
        s.write_all(&frame(1, &hello)).unwrap();
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
        opt(&mut o, grant);
        opt(&mut o, key);
        s.write_all(&frame(2, &o)).unwrap();
        let mut r = Raw { s, buf: Vec::new() };
        assert_eq!(r.next().0, 1, "hello first");
        r
    }

    /// The next frame: (tag, payload).
    fn next(&mut self) -> (u8, Vec<u8>) {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                    as usize;
                if self.buf.len() >= 4 + len {
                    let f: Vec<u8> = self.buf.drain(..4 + len).collect();
                    return (f[4], f[5..].to_vec());
                }
            }
            let mut chunk = [0u8; 256];
            let n = self.s.read(&mut chunk).expect("read");
            assert!(n > 0, "daemon hung up");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// `opened`: (root pid, reattached).
    fn opened(&mut self) -> (i32, bool) {
        let (tag, p) = self.next();
        assert_eq!(tag, 3, "opened, got tag {tag} {p:?}");
        (i32::from_le_bytes([p[0], p[1], p[2], p[3]]), p[4] != 0)
    }
}

fn read_pid(p: &Path) -> i32 {
    std::fs::read_to_string(p).unwrap().trim().parse().unwrap()
}

/// A granted, keyed `cterm` whose child records its pid in `ready` and loops; the fake Wayland
/// connection is lost after `loss` ms and back `back` ms later (when given).
fn keyed(e: &Env, ready: &Path, loss: Option<&str>, back: Option<&str>) -> Command {
    let mut c = e.sh(&format!(
        "trap 'exit 0' HUP; echo $$ > {}; while :; do sleep 0.05; done",
        ready.display()
    ));
    c.env("CRUCIBLE_WINDOW_GRANT", "g")
        .env("CRUCIBLE_TERM_SESSION", KEY)
        .env("CTERM_DEBUG_HEADLESS_IDENTITY", "1");
    if let Some(l) = loss {
        c.env("CTERM_DEBUG_HEADLESS_LOSS_MS", l);
    }
    if let Some(b) = back {
        c.env("CTERM_DEBUG_HEADLESS_RECONNECT_MS", b);
    }
    c
}

fn wait_log(e: &Env, needle: &str) {
    let t = Instant::now();
    while !e.log().contains(needle) && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(e.log().contains(needle), "{needle:?} not in {}", e.log());
}

/// Reattach after a loss and reconnect: the new granted open answers `reattached` with the
/// same root pid, the old client is superseded and exits 0, and (pin E1) its EOF leaves the
/// child alive and owned by the new connection, which can still close it.
#[test]
fn a_detached_session_is_reattached_by_key_and_the_old_client_superseded() {
    let e = Env::new("reattach");
    let ready = e.root.join("ready");
    let old = keyed(&e, &ready, Some("300"), Some("200"))
        .spawn()
        .expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)));
    let child = read_pid(&ready);
    wait_log(&e, "daemon: reconnected");
    let mut new = Raw::open(&e, "exit 9", Some("g"), Some(KEY));
    assert_eq!(new.opened(), (child, true));
    let out = old.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // E1: the superseded client's EOF did not close the session.
    std::thread::sleep(Duration::from_millis(700));
    assert!(alive(child), "old-client EOF closed the reattached session");
    new.s.write_all(&frame(6, &[])).unwrap();
    assert_eq!(new.next().0, 5, "the new owner gets the exit");
}

/// A key naming an attached session is never stolen.
#[test]
fn an_attached_key_is_not_stolen() {
    let e = Env::new("nosteal");
    let ready = e.root.join("ready");
    let mut a = keyed(&e, &ready, None, None).spawn().expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)));
    let child = read_pid(&ready);
    let mut b = Raw::open(&e, "sleep 5", Some("g"), Some(KEY));
    let (pid, re) = b.opened();
    assert!(!re && pid != child);
    assert!(
        e.log().contains("already names live session"),
        "{}",
        e.log()
    );
    assert!(
        a.try_wait().unwrap().is_none(),
        "old client kept its window"
    );
    let _ = a.kill();
    let _ = a.wait();
}

/// A key without a grant never reattaches, even a detached session.
#[test]
fn a_key_without_a_grant_never_reattaches() {
    let e = Env::new("nogrant");
    let ready = e.root.join("ready");
    let mut a = keyed(&e, &ready, Some("200"), Some("100"))
        .spawn()
        .expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)));
    let child = read_pid(&ready);
    wait_log(&e, "daemon: reconnected");
    let mut b = Raw::open(&e, "sleep 5", None, Some(KEY));
    let (pid, re) = b.opened();
    assert!(!re && pid != child);
    assert!(e.log().contains("without a grant"), "{}", e.log());
    assert!(a.try_wait().unwrap().is_none(), "not superseded");
    let _ = a.kill();
    let _ = a.wait();
}

/// `detached-timeout 1`: a session still detached 1 s after the reconnect gets SIGHUP.
#[test]
fn an_unclaimed_detached_session_is_closed_after_the_timeout() {
    let e = Env::new("timeout");
    std::fs::write(
        e.root.join("cfg/crucible-term/config.kdl"),
        "detached-timeout 1\n",
    )
    .unwrap();
    let ready = e.root.join("ready");
    let hup = e.root.join("hup");
    let mut c = e.sh(&format!(
        "trap 'echo > {}; exit 0' HUP; echo $$ > {}; while :; do sleep 0.05; done",
        hup.display(),
        ready.display()
    ));
    c.env("CTERM_DEBUG_HEADLESS_LOSS_MS", "200")
        .env("CTERM_DEBUG_HEADLESS_RECONNECT_MS", "100");
    let c = c.spawn().expect("client");
    assert!(wait_for(&hup, Duration::from_secs(6)), "{}", e.log());
    assert!(
        e.log().contains("unclaimed after 1s; closing"),
        "{}",
        e.log()
    );
    assert_eq!(c.wait_with_output().unwrap().status.code(), Some(0));
}

/// The session key reaches neither the daemon's nor the child's environment.
#[test]
fn the_session_key_never_reaches_the_daemon_or_the_child() {
    let e = Env::new("keyenv");
    let dump = e.root.join("env");
    let mut c = e.sh(&format!("env > {}", dump.display()));
    c.env("CRUCIBLE_WINDOW_GRANT", "g")
        .env("CRUCIBLE_TERM_SESSION", KEY)
        .env("CTERM_DEBUG_HEADLESS_IDENTITY", "1");
    assert_eq!(status(c).0, 0);
    let env = std::fs::read_to_string(&dump).unwrap();
    assert!(
        env.contains("PATH=") && !env.contains("CRUCIBLE_TERM_SESSION"),
        "{env}"
    );
    let pid = e.daemon_pid().expect("daemon");
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(!String::from_utf8_lossy(&environ).contains("CRUCIBLE_TERM_SESSION"));
}

// ---- durable session records (`scrollback-persist`) ----

impl Env {
    /// An env whose daemon persists records on the short TEST-ONLY cadence (quiet 200 ms,
    /// max age 1 s, deletion grace 800 ms) and logs every save.
    fn persisting(name: &str) -> Env {
        let e = Env::new(name);
        e.config("scrollback-persist #true\n");
        e
    }

    fn config(&self, kdl: &str) {
        std::fs::write(self.root.join("cfg/crucible-term/config.kdl"), kdl).unwrap();
    }

    fn records(&self) -> PathBuf {
        self.root.join("state/crucible-term/sessions")
    }

    fn record(&self) -> PathBuf {
        self.records().join(format!("{KEY}.vt"))
    }

    fn record_text(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.record()).unwrap_or_default()).into_owned()
    }

    /// A granted, keyed `cterm` running `script` (which should end in a loop that exits on HUP).
    fn keyed_sh(&self, script: &str) -> Command {
        let mut c = self.sh(script);
        c.env("CRUCIBLE_WINDOW_GRANT", "g")
            .env("CRUCIBLE_TERM_SESSION", KEY)
            .env("CTERM_DEBUG_HEADLESS_IDENTITY", "1")
            .env("CTERM_DEBUG_PERSIST_MS", "200,1000,800");
        c
    }
}

fn until(within: Duration, f: impl Fn() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn signal(pid: i32, sig: rustix::process::Signal) {
    let _ = rustix::process::kill_process(rustix::process::Pid::from_raw(pid).unwrap(), sig);
}

/// A loop that records its pid in `ready` and exits 0 on SIGHUP.
fn hup_loop(ready: &Path) -> String {
    format!(
        "trap 'exit 0' HUP; echo $$ > {}; while :; do sleep 0.05; done",
        ready.display()
    )
}

/// Off by default: a keyed session writes nothing and the records directory is never created.
#[test]
fn records_are_off_by_default() {
    let e = Env::new("persist-off");
    let ready = e.root.join("ready");
    let mut c = e
        .keyed_sh(&format!("echo MARK; {}", hup_loop(&ready)))
        .spawn()
        .expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(600));
    assert!(!e.records().exists(), "{}", e.log());
    let _ = c.kill();
    let _ = c.wait();
}

/// A keyed session is saved once it goes quiet (0600 in a 0700 directory, versioned header),
/// is not rewritten while unchanged, and a real close deletes it after the grace.
#[test]
fn a_record_is_written_once_while_unchanged_and_deleted_after_close() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::persisting("persist-write");
    let ready = e.root.join("ready");
    let c = e
        .keyed_sh(&format!("echo MARK-ONE; {}", hup_loop(&ready)))
        .spawn()
        .expect("client");
    assert!(
        until(Duration::from_secs(5), || e
            .record_text()
            .contains("MARK-ONE")),
        "{}",
        e.log()
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&e.record()), mode(&e.records())), (0o600, 0o700));
    assert!(
        e.record_text()
            .starts_with(&format!("cterm-session 1 key={KEY} cols=80 rows=24 ")),
        "{}",
        e.record_text()
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(e.log().matches("saved record").count(), 1, "{}", e.log());
    // SIGTERM to the client sends `close`: a real close.
    signal(c.id() as i32, rustix::process::Signal::TERM);
    let _ = c.wait_with_output();
    assert!(
        until(Duration::from_secs(5), || !e.record().exists()),
        "{}",
        e.log()
    );
    assert!(e.log().contains(&format!("deleted record {KEY}")));
}

/// While the alternate screen is active the last primary-screen record stands.
#[test]
fn the_alternate_screen_is_never_saved() {
    let e = Env::persisting("persist-alt");
    let ready = e.root.join("ready");
    let mut c = e
        .keyed_sh(&format!(
            "echo PRIMARY; sleep 0.6; printf '\\033[?1049hALTSCREEN'; {}",
            hup_loop(&ready)
        ))
        .spawn()
        .expect("client");
    assert!(
        until(Duration::from_secs(5), || e
            .record_text()
            .contains("PRIMARY")),
        "{}",
        e.log()
    );
    assert!(wait_for(&ready, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !e.record_text().contains("ALTSCREEN"),
        "{}",
        e.record_text()
    );
    let _ = c.kill();
    let _ = c.wait();
}

/// SIGTERM to the daemon keeps the record; the next keyed open (a new daemon) replays it above
/// the new child's output, and the next save carries both.
#[test]
fn sigterm_keeps_the_record_and_the_next_keyed_open_replays_it() {
    let e = Env::persisting("persist-term");
    let ready = e.root.join("ready");
    let mut c = e
        .keyed_sh(&format!("echo OLD-HISTORY; {}", hup_loop(&ready)))
        .spawn()
        .expect("client");
    assert!(until(Duration::from_secs(5), || e
        .record_text()
        .contains("OLD-HISTORY")));
    let pid = e.daemon_pid().expect("daemon");
    signal(pid, rustix::process::Signal::TERM);
    assert!(until(Duration::from_secs(5), || !alive(pid)), "{}", e.log());
    let _ = c.kill();
    let _ = c.wait();
    assert!(
        e.log().contains("SIGTERM: saving session records"),
        "{}",
        e.log()
    );
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        e.record_text().contains("OLD-HISTORY"),
        "kept after the grace"
    );
    let ready2 = e.root.join("ready2");
    let mut c2 = e
        .keyed_sh(&format!("echo NEW-CHILD; {}", hup_loop(&ready2)))
        .spawn()
        .expect("client");
    wait_log(&e, "from record (");
    assert!(
        e.log().contains("restored session 1 from record ("),
        "{}",
        e.log()
    );
    assert!(until(Duration::from_secs(5), || e
        .record_text()
        .contains("NEW-CHILD")));
    let t = e.record_text();
    let (old, sep, new) = (
        t.find("OLD-HISTORY").unwrap(),
        t.find("restored from record").unwrap(),
        t.find("NEW-CHILD").unwrap(),
    );
    assert!(old < sep && sep < new, "{t}");
    let _ = c2.kill();
    let _ = c2.wait();
}

/// A reboot's SIGHUP reaches the daemon and the shell at once: the child's exit must not count
/// as a close, so the record survives.
#[test]
fn sighup_arriving_with_the_childs_exit_keeps_the_record() {
    let e = Env::persisting("persist-hup");
    let ready = e.root.join("ready");
    let mut c = e
        .keyed_sh(&format!("echo HUP-MARK; {}", hup_loop(&ready)))
        .spawn()
        .expect("client");
    assert!(until(Duration::from_secs(5), || e
        .record_text()
        .contains("HUP-MARK")));
    let pid = e.daemon_pid().expect("daemon");
    signal(read_pid(&ready), rustix::process::Signal::HUP);
    signal(pid, rustix::process::Signal::HUP);
    assert!(until(Duration::from_secs(5), || !alive(pid)), "{}", e.log());
    std::thread::sleep(Duration::from_millis(1200));
    assert!(e.record_text().contains("HUP-MARK"), "{}", e.log());
    let _ = c.kill();
    let _ = c.wait();
}

/// Reattach and no-steal keep priority: neither replays, and the no-steal session writes nothing.
#[test]
fn reattach_and_no_steal_never_replay() {
    let e = Env::persisting("persist-prio");
    let ready = e.root.join("ready");
    let old = e
        .keyed_sh(&format!("echo FIRST; {}", hup_loop(&ready)))
        .env("CTERM_DEBUG_HEADLESS_LOSS_MS", "600")
        .env("CTERM_DEBUG_HEADLESS_RECONNECT_MS", "200")
        .spawn()
        .expect("client");
    assert!(until(Duration::from_secs(5), || e
        .record_text()
        .contains("FIRST")));
    // No-steal (still attached): a second session, not registered, never writes.
    let mut b = Raw::open(&e, "echo INTRUDER; sleep 5", Some("g"), Some(KEY));
    assert!(!b.opened().1);
    wait_log(&e, "daemon: reconnected");
    let mut r = Raw::open(&e, "exit 9", Some("g"), Some(KEY));
    assert!(r.opened().1, "reattached");
    std::thread::sleep(Duration::from_millis(600));
    assert!(!e.log().contains("from record"), "{}", e.log());
    assert!(!e.record_text().contains("INTRUDER"), "{}", e.record_text());
    drop(b);
    r.s.write_all(&frame(6, &[])).unwrap();
    let _ = old.wait_with_output();
}

/// E2: a detached-timeout close is an abandoned session, and an orphan-grace close (the client
/// died while the display was up) a real one: both records are deleted after the grace, even
/// though the timeout clears the reattach key first.
#[test]
fn timeout_and_orphan_closes_delete_the_record() {
    let e = Env::persisting("persist-e2");
    e.config("scrollback-persist #true\ndetached-timeout 1\n");
    let ready = e.root.join("ready");
    let c = e
        .keyed_sh(&format!("echo TIMEOUT; {}", hup_loop(&ready)))
        .env("CTERM_DEBUG_HEADLESS_LOSS_MS", "600")
        .env("CTERM_DEBUG_HEADLESS_RECONNECT_MS", "100")
        .spawn()
        .expect("client");
    assert!(until(Duration::from_secs(5), || e
        .record_text()
        .contains("TIMEOUT")));
    wait_log(&e, "unclaimed after 1s; closing");
    assert!(
        until(Duration::from_secs(5), || !e.record().exists()),
        "{}",
        e.log()
    );
    let _ = c.wait_with_output();
    // Orphan: the client is SIGKILLed while connected.
    let ready2 = e.root.join("ready2");
    let mut c2 = e
        .keyed_sh(&format!("echo ORPHAN; {}", hup_loop(&ready2)))
        .spawn()
        .expect("client");
    assert!(
        until(Duration::from_secs(5), || e
            .record_text()
            .contains("ORPHAN")),
        "{}",
        e.log()
    );
    let _ = c2.kill();
    let _ = c2.wait();
    assert!(
        until(Duration::from_secs(8), || !e.record().exists()),
        "{}",
        e.log()
    );
}

/// The startup sweep deletes aged records and dead daemons' temp files; a record of an unknown
/// version is skipped with a log line, left on disk, and the session opens fresh.
#[test]
fn the_sweep_and_an_unknown_version() {
    let e = Env::persisting("persist-sweep");
    std::fs::create_dir_all(e.records()).unwrap();
    let old = e.records().join("feedfeedfeedfeedfeedfeedfeedfeed.vt");
    std::fs::write(&old, b"cterm-session 1 key=x cols=1 rows=1\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(30 * 86_400))
        .unwrap();
    let tmp = e.records().join(format!(".{KEY}.vt.999999999.tmp"));
    std::fs::write(&tmp, b"x").unwrap();
    std::fs::write(
        e.record(),
        format!("cterm-session 9 key={KEY} cols=80 rows=24\nFUTURE"),
    )
    .unwrap();
    let ready = e.root.join("ready");
    let mut c = e
        .keyed_sh(&format!("sleep 0.3; {}", hup_loop(&ready)))
        .spawn()
        .expect("client");
    assert!(wait_for(&ready, Duration::from_secs(5)));
    assert!(!old.exists() && !tmp.exists(), "{}", e.log());
    assert!(
        e.log().contains("record skipped:") && e.log().contains("unknown version"),
        "{}",
        e.log()
    );
    assert!(!e.log().contains("from record ("), "{}", e.log());
    let _ = c.kill();
    let _ = c.wait();
}

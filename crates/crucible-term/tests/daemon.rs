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
    assert!(wait_for(&hup, Duration::from_secs(3)), "child got SIGHUP");
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
    let mut hello = 1u32.to_le_bytes().to_vec();
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
    open.extend_from_slice(&[0, 0, 0]);
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

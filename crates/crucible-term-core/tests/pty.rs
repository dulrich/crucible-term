//! The owned PTY.
use crucible_term_core::Session;
use crucible_term_core::pty::{Pty, SpawnSpec, inherited_env};

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: "/tmp".into(),
        env: inherited_env(),
        size: (80, 24),
    }
}

#[test]
fn spawn_reports_output_and_exit_status() {
    let mut p = Pty::spawn(&spec(
        "printf 'hi %s|%s|%s' \"$TERM\" \"$(pwd)\" \"$(stty size)\"; exit 3",
    ))
    .expect("spawn");
    let mut out = Vec::new();
    p.read_to_end(&mut out).expect("read");
    let st = p.wait().expect("wait");
    assert_eq!(st.code(), Some(3));
    let mut s = Session::new(80, 24, 0);
    s.feed(&out);
    assert_eq!(s.screen_row(0).text(), "hi xterm-256color|/tmp|24 80");
}

#[test]
fn resize_reaches_the_child_and_fds_are_swept() {
    let mut p = Pty::spawn(&spec("read _; stty size; ls /proc/self/fd | wc -l")).expect("spawn");
    p.resize((132, 40)).expect("resize");
    p.write_all(b"\n").expect("write");
    let mut out = Vec::new();
    p.read_to_end(&mut out).expect("read");
    assert_eq!(p.wait().expect("wait").code(), Some(0));
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("40 132"), "{text:?}");
    // ls sees 0, 1, 2 and its own directory fd: nothing leaked from the parent.
    assert!(text.lines().any(|l| l.trim() == "4"), "{text:?}");
}

#[test]
fn empty_argv_is_an_error() {
    let mut s = spec("");
    s.argv.clear();
    assert!(Pty::spawn(&s).is_err());
}

/// A parent that blocks SIGTERM (as the daemon does for its signalfd) never hands the block to
/// the child: the child's `SigBlk` is empty.
#[test]
fn the_child_starts_with_an_empty_signal_mask() {
    std::thread::spawn(|| {
        #[allow(unsafe_code)]
        // SAFETY: plain libc calls on a stack sigset, on this test's own thread.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGHUP);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        }
        let mut p = Pty::spawn(&spec("grep SigBlk /proc/self/status")).expect("spawn");
        let mut out = Vec::new();
        p.read_to_end(&mut out).expect("read");
        assert_eq!(p.wait().expect("wait").code(), Some(0));
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("SigBlk:\t0000000000000000"), "{text:?}");
    })
    .join()
    .expect("thread");
}

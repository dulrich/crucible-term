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

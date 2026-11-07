use super::*;
use smithay_client_toolkit::reexports::calloop::timer::Timer;
use std::time::Instant;

fn host(detachable: bool, display: Option<PathBuf>) -> (EventLoop<'static, Host>, Host) {
    let ev: EventLoop<Host> = EventLoop::try_new().expect("event loop");
    let h = Host::detached(ev.handle(), detachable, display);
    (ev, h)
}

/// Stands in for sctk's keyboard `RepeatData`: its `Drop` removes another loop source.
struct RemovesOnDrop(LoopHandle<'static, Host>, Option<RegistrationToken>);
impl Drop for RemovesOnDrop {
    fn drop(&mut self) {
        if let Some(t) = self.1.take() {
            self.0.remove(t);
        }
    }
}

/// A source whose callback owns a [`RemovesOnDrop`] guarding a second timer.
fn guarded(
    h: &Host,
) -> (
    Timer,
    impl FnMut(Instant, &mut (), &mut Host) -> TimeoutAction + use<>,
) {
    let repeat = h
        .handle
        .insert_source(Timer::from_duration(Duration::from_secs(60)), |_, _, _| {
            TimeoutAction::Drop
        })
        .expect("repeat timer");
    let guard = RemovesOnDrop(h.handle.clone(), Some(repeat));
    let source = Timer::from_duration(Duration::from_secs(60));
    (source, move |_, _, _| {
        let _ = &guard;
        TimeoutAction::Drop
    })
}

/// The live-session logout panic: removing a source by token alone drops it inside
/// `LoopHandle::remove`, and a `Drop` that removes another source panics.
#[test]
#[should_panic(expected = "already borrowed")]
fn removing_by_token_drops_the_source_inside_remove() {
    let (_ev, h) = host(true, None);
    let (source, cb) = guarded(&h);
    let token = h.handle.insert_source(source, cb).expect("insert");
    h.handle.remove(token);
}

/// [`Registered::remove`] drops the source after `remove` returns, so the nested remove
/// (sctk's armed key-repeat timer) runs without a panic.
#[test]
fn registered_remove_drops_the_source_after_unregistering() {
    let (mut ev, mut h) = host(true, None);
    let (source, cb) = guarded(&h);
    let reg = Registered::insert(&h.handle, source, cb).expect("insert");
    reg.remove(&h.handle);
    ev.dispatch(Some(Duration::ZERO), &mut h)
        .expect("loop still usable");
}

/// A surface-less session on `sh -c script`, as a detached daemon holds it.
fn spawn(h: &mut Host, script: &str) -> TermId {
    let cfg = Config::default();
    let pty = Pty::spawn(&SpawnSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: cfg.cwd.clone(),
        env: vec![("PATH".into(), std::env::var_os("PATH").unwrap_or_default())],
        size: (cfg.cols, cfg.rows),
    })
    .expect("spawn");
    let id = TermId(h.next_id);
    h.next_id += 1;
    let t = h.new_session(id, &cfg, pty).expect("session");
    h.terms.insert(id, t);
    id
}

/// Dispatch until `id`'s exit is recorded (or `within` passes); returns it.
fn run_until_exit(
    ev: &mut EventLoop<'static, Host>,
    h: &mut Host,
    id: TermId,
    within: Duration,
) -> Option<i32> {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        ev.dispatch(Some(Duration::from_millis(50)), h)
            .expect("dispatch");
        h.reap();
        if let Some(c) = h.exits.get(&id) {
            return Some(*c);
        }
    }
    None
}

/// A loss keeps every session: the PTY sources stay registered, the `Session` stays, and
/// the session reports detached; the host is lost until a reconnect.
#[test]
fn a_session_survives_a_loss() {
    let (mut ev, mut h) = host(true, None);
    let id = spawn(&mut h, "printf ready; exec cat");
    assert!(h.detach(), "daemon mode detaches");
    assert!(h.is_lost() && h.is_detached(id));
    for _ in 0..5 {
        ev.dispatch(Some(Duration::from_millis(20)), &mut h)
            .unwrap();
        h.reap();
    }
    let t = h.terms.get(&id).expect("session kept");
    assert!(t.exit.is_none() && t.surf.is_none());
    assert_eq!(t.sources.len(), 2, "PTY and pidfd sources kept");
    // Cleanup: the session closes through the grace (our own child).
    h.close(id);
    h.reap();
    assert!(run_until_exit(&mut ev, &mut h, id, Duration::from_secs(5)).is_some());
}

/// Detached, the session keeps draining and parsing its PTY: a 4 MiB writer completes (it
/// would block forever on a full PTY) and its exit code is recorded for the relay.
#[test]
fn a_detached_session_drains_a_flooding_child_and_records_its_exit() {
    let (mut ev, mut h) = host(true, None);
    let id = spawn(&mut h, "head -c 4194304 /dev/zero | tr '\\0' x; exit 7");
    assert!(h.detach());
    assert_eq!(
        run_until_exit(&mut ev, &mut h, id, Duration::from_secs(30)),
        Some(7)
    );
    assert!(h.terms.is_empty(), "the exited session is removed");
}

/// Standalone never detaches: the loss ends the loop and nothing is dropped.
#[test]
fn standalone_does_not_detach() {
    let (mut ev, mut h) = host(false, None);
    let id = spawn(&mut h, "exec cat");
    assert!(!h.detach(), "standalone: the loss is the loop error");
    assert!(h.terms.contains_key(&id));
    h.close(id);
    h.reap();
    assert!(run_until_exit(&mut ev, &mut h, id, Duration::from_secs(5)).is_some());
}

/// Reconnect targets the captured display only; an unreachable one leaves the host lost,
/// and an attach while lost is refused without touching the session.
#[test]
fn reconnect_to_a_missing_display_stays_detached() {
    let gone = std::env::temp_dir().join(format!("cterm-no-display-{}", std::process::id()));
    let (mut ev, mut h) = host(true, Some(gone));
    let id = spawn(&mut h, "exec cat");
    assert!(!h.reconnect());
    assert!(h.is_lost());
    assert!(h.attach(id, None).is_err());
    assert!(h.is_detached(id));
    h.close(id);
    h.reap();
    assert!(run_until_exit(&mut ev, &mut h, id, Duration::from_secs(5)).is_some());
}

#[test]
fn display_path_follows_libwayland() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    };
    assert_eq!(
        display_path(&env(&[
            ("XDG_RUNTIME_DIR", "/run/u"),
            ("WAYLAND_DISPLAY", "w-1")
        ])),
        Some(PathBuf::from("/run/u/w-1"))
    );
    assert_eq!(
        display_path(&env(&[("XDG_RUNTIME_DIR", "/run/u")])),
        Some(PathBuf::from("/run/u/wayland-0"))
    );
    assert_eq!(
        display_path(&env(&[("WAYLAND_DISPLAY", "/abs/w")])),
        Some(PathBuf::from("/abs/w"))
    );
    assert_eq!(display_path(&env(&[("WAYLAND_DISPLAY", "w")])), None);
}

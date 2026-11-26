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

// ---- a size the window did not set ------------------------------------------------------

/// What the PTY master reports as its window size, `(cols, rows)`.
fn pty_size(h: &Host, id: TermId) -> (u16, u16) {
    let w = rustix::termios::tcgetwinsize(h.terms[&id].pty.master()).expect("TIOCGWINSZ");
    (w.ws_col, w.ws_row)
}

/// Give the surface-less session a buffer, as a configure would, and return the grid the
/// window's own buffer derives.
fn give_buffer(h: &mut Host, id: TermId, buf: (u32, u32)) -> (usize, usize) {
    let t = h.terms.get_mut(&id).expect("term");
    t.logical = buf;
    t.buf = buf;
    let pad = t.cfg.settings.pad;
    crate::render::grid_size(
        buf.0,
        buf.1,
        &t.metrics(),
        crate::render::Pad { x: pad.0, y: pad.1 },
    )
}

fn close(ev: &mut EventLoop<'static, Host>, h: &mut Host, id: TermId) {
    h.close(id);
    h.reap();
    assert!(run_until_exit(ev, h, id, Duration::from_secs(5)).is_some());
}

/// `Some` sizes session and PTY exactly; a window resize afterwards (relayout) changes
/// nothing; `None` hands the size back and the session returns to the window's grid.
#[test]
fn a_viewer_size_survives_a_window_resize_until_handed_back() {
    let (mut ev, mut h) = host(true, None);
    let id = spawn(&mut h, "exec cat");
    let window = give_buffer(&mut h, id, (800, 600));
    assert!(h.set_size(id, Some((37, 11))).is_ok());
    assert_eq!(h.terms[&id].session.size(), (37, 11));
    assert_eq!(pty_size(&h, id), (37, 11));
    // A simulated buffer resize and a relayout (a configure or a scale change).
    let bigger = give_buffer(&mut h, id, (1000, 700));
    assert_ne!(bigger, window);
    let t = h.terms.get_mut(&id).expect("term");
    t.relayout(&mut h.fonts);
    assert_eq!(
        t.session.size(),
        (37, 11),
        "the window did not change the session"
    );
    assert_eq!(pty_size(&h, id), (37, 11));
    // Hand it back: the window's own grid.
    assert!(h.set_size(id, None).is_ok());
    assert_eq!(h.terms[&id].session.size(), bigger);
    assert_eq!(pty_size(&h, id), (bigger.0 as u16, bigger.1 as u16));
    close(&mut ev, &mut h, id);
}

/// An unknown session is an error, never a silent success; zero clamps to 1x1.
#[test]
fn set_size_rejects_unknown_ids_and_clamps_zero() {
    let (mut ev, mut h) = host(true, None);
    let id = spawn(&mut h, "exec cat");
    assert!(h.set_size(TermId(999), Some((10, 10))).is_err());
    assert!(h.set_size(TermId(999), None).is_err());
    assert!(h.set_size(id, Some((0, 0))).is_ok());
    assert_eq!(h.terms[&id].session.size(), (1, 1));
    assert_eq!(pty_size(&h, id), (1, 1));
    close(&mut ev, &mut h, id);
}

/// A detached session (no surface) takes `Some` and keeps its size on `None`, even after a
/// loss zeroed its buffer.
#[test]
fn a_detached_session_accepts_both_requests() {
    let (mut ev, mut h) = host(true, None);
    let id = spawn(&mut h, "exec cat");
    give_buffer(&mut h, id, (800, 600));
    assert!(h.detach());
    assert!(h.set_size(id, Some((50, 20))).is_ok());
    assert_eq!(h.terms[&id].session.size(), (50, 20));
    assert_eq!(pty_size(&h, id), (50, 20));
    assert!(h.set_size(id, None).is_ok());
    assert_eq!(h.terms[&id].session.size(), (50, 20), "None keeps the grid");
    assert_eq!(pty_size(&h, id), (50, 20));
    close(&mut ev, &mut h, id);
}

/// A key press in the window takes the size back before its bytes are encoded and raises the
/// daemon's flag exactly once; with no foreign size it raises nothing, and a window resize is
/// not activity.
#[test]
fn a_key_press_reclaims_the_size_once() {
    use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};
    let (mut ev, mut h) = host(true, None);
    let id = spawn(&mut h, "exec cat");
    let window = give_buffer(&mut h, id, (800, 600));
    let key = KeyEvent {
        time: 0,
        raw_code: 30,
        keysym: Keysym::a,
        utf8: Some("a".into()),
    };
    let mut seat = SeatInput::detached();
    let press = |h: &mut Host, seat: &mut SeatInput| {
        h.terms
            .get_mut(&id)
            .expect("term")
            .on_key(seat, &key, false);
    };
    press(&mut h, &mut seat);
    assert!(h.take_window_active().is_empty(), "no foreign size");
    assert!(h.set_size(id, Some((30, 9))).is_ok());
    h.terms.get_mut(&id).expect("term").relayout(&mut h.fonts);
    assert!(
        h.take_window_active().is_empty(),
        "a resize is not activity"
    );
    assert_eq!(h.terms[&id].session.size(), (30, 9));
    press(&mut h, &mut seat);
    assert_eq!(h.terms[&id].session.size(), window);
    assert_eq!(pty_size(&h, id), (window.0 as u16, window.1 as u16));
    assert_eq!(h.take_window_active(), vec![id]);
    press(&mut h, &mut seat);
    assert!(h.take_window_active().is_empty(), "exactly once");
    close(&mut ev, &mut h, id);
}

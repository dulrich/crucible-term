//! Display-buffer lifecycle across xdg_toplevel `suspended` (the compositor hid the window).
//!
//! While suspended a window draws nothing, shows a 1×1 placeholder buffer, and gives its
//! full-size shm buffers and their pool back to the OS. The ordering rule is the one that
//! matters: a buffer the compositor still holds (attached, not yet `wl_buffer.release`d) is never
//! destroyed or reused, and the pool is only recreated once no full-size buffer is held (else on
//! the last release). Pure: no Wayland types, so it unit-tests; [`super::term::Term`] runs every
//! decision through it.

/// Whether the compositor still holds a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BufState {
    /// Released (or never attached): safe to destroy or reuse.
    Released,
    /// Attached and not yet released: must not be destroyed or reused.
    Held,
}

/// What a configure's `suspended` flag means for this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transition {
    /// No change of the suspended state (always the case on a compositor that never sends it).
    None,
    /// Visible → suspended.
    Suspend,
    /// Suspended → visible.
    Resume,
}

/// The decision on suspend for the live full-size buffers (indices into the caller's list).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct SuspendPlan {
    /// Held buffers: keep until their release is observed, then destroy.
    pub drain: Vec<usize>,
    /// Nothing is held: recreate (empty) the pool now.
    pub recreate_pool: bool,
}

/// Per-window suspended state plus the number of held buffers still draining.
#[derive(Debug, Default)]
pub(super) struct Lifecycle {
    suspended: bool,
    draining: usize,
    /// The pool still holds the pre-suspend mapping and must be recreated once drained.
    pool_stale: bool,
}

impl Lifecycle {
    pub(super) fn suspended(&self) -> bool {
        self.suspended
    }

    /// Held buffers whose release has not been observed yet.
    pub(super) fn draining(&self) -> usize {
        self.draining
    }

    /// Drawing is allowed only while visible.
    pub(super) fn may_draw(&self) -> bool {
        !self.suspended
    }

    /// Classify a configure by its `suspended` flag (transitions only).
    pub(super) fn transition(&self, suspended: bool) -> Transition {
        match (self.suspended, suspended) {
            (false, true) => Transition::Suspend,
            (true, false) => Transition::Resume,
            _ => Transition::None,
        }
    }

    /// Enter the suspended state with these live full-size buffers.
    pub(super) fn suspend(&mut self, bufs: &[BufState]) -> SuspendPlan {
        if self.suspended {
            return SuspendPlan::default();
        }
        self.suspended = true;
        let mut plan = SuspendPlan::default();
        for (i, s) in bufs.iter().enumerate() {
            // Released buffers need no entry: the caller destroys whatever is not drained.
            if *s == BufState::Held {
                plan.drain.push(i);
            }
        }
        self.draining += plan.drain.len();
        plan.recreate_pool = self.draining == 0;
        self.pool_stale = !plan.recreate_pool;
        plan
    }

    /// A draining buffer was released (it may now be destroyed). Returns whether the pool may be
    /// recreated now: the last held buffer is gone and the window is still suspended.
    pub(super) fn release(&mut self) -> bool {
        self.draining = self.draining.saturating_sub(1);
        if self.draining == 0 && self.pool_stale && self.suspended {
            self.pool_stale = false;
            return true;
        }
        false
    }

    /// Leave the suspended state; `false` if the window was not suspended. Buffers still
    /// draining keep draining (the pool is in use again, so it is no longer recreated). The
    /// caller then forgets its stale frame callback and repaints in full.
    pub(super) fn resume(&mut self) -> bool {
        if !self.suspended {
            return false;
        }
        self.suspended = false;
        self.pool_stale = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::BufState::{Held, Released};
    use super::*;

    #[test]
    fn suspend_destroys_only_released_and_drains_held() {
        let mut l = Lifecycle::default();
        let p = l.suspend(&[Released, Held]);
        assert_eq!(p.drain, vec![1]);
        assert!(!p.recreate_pool, "a held buffer still maps the pool");
        assert!(!l.may_draw());
        assert_eq!(l.draining(), 1);
        // The last release recreates the pool, exactly once.
        assert!(l.release());
        assert_eq!(l.draining(), 0);
        assert!(!l.release());
    }

    #[test]
    fn suspend_with_nothing_held_recreates_the_pool_at_once() {
        let mut l = Lifecycle::default();
        let p = l.suspend(&[Released, Released]);
        assert!(p.drain.is_empty());
        assert!(p.recreate_pool);
        let p = Lifecycle::default().suspend(&[]);
        assert!(p.recreate_pool);
    }

    #[test]
    fn pool_waits_for_the_last_of_several_releases() {
        let mut l = Lifecycle::default();
        let p = l.suspend(&[Held, Held]);
        assert_eq!(p.drain, vec![0, 1]);
        assert!(!l.release());
        assert!(l.release());
    }

    #[test]
    fn release_after_resume_never_recreates_the_pool() {
        let mut l = Lifecycle::default();
        l.suspend(&[Held]);
        assert!(l.resume());
        assert!(!l.release(), "the pool is in use again");
        assert_eq!(l.draining(), 0);
    }

    #[test]
    fn suspend_resume_round_trip_forces_a_full_repaint() {
        let mut l = Lifecycle::default();
        assert_eq!(l.transition(true), Transition::Suspend);
        l.suspend(&[Held]);
        assert_eq!(l.transition(true), Transition::None, "repeat suspend");
        assert_eq!(l.suspend(&[Held]), SuspendPlan::default());
        assert_eq!(l.transition(false), Transition::Resume);
        assert!(l.resume());
        assert!(l.may_draw());
        assert!(!l.resume(), "already visible");
    }

    #[test]
    fn no_suspend_configure_changes_nothing() {
        let l = Lifecycle::default();
        for _ in 0..3 {
            assert_eq!(l.transition(false), Transition::None);
        }
        assert!(l.may_draw());
        assert!(!l.suspended());
        assert_eq!(l.draining(), 0);
    }
}

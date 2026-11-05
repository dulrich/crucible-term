//! Pure routing for a multi-window host: which [`TermId`] owns a surface, and which one holds
//! keyboard focus. Surfaces are an opaque key `K` (the host uses the Wayland object id), so the
//! table unit-tests without a compositor. Key presses and key repeat go to the focused window;
//! OSC 52 is honoured only for the window holding keyboard focus.

use std::collections::HashMap;
use std::hash::Hash;

/// One window (`Term`) of a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TermId(pub u64);

/// Surface → window table plus keyboard focus.
#[derive(Debug)]
pub struct Routes<K> {
    by_surface: HashMap<K, TermId>,
    focus: Option<TermId>,
}

impl<K> Default for Routes<K> {
    fn default() -> Self {
        Routes {
            by_surface: HashMap::new(),
            focus: None,
        }
    }
}

impl<K: Eq + Hash> Routes<K> {
    /// File `surface` under `id`.
    pub fn insert(&mut self, surface: K, id: TermId) {
        self.by_surface.insert(surface, id);
    }

    /// Forget every surface of `id`; it loses keyboard focus if it held it.
    pub fn remove_term(&mut self, id: TermId) {
        self.by_surface.retain(|_, t| *t != id);
        if self.focus == Some(id) {
            self.focus = None;
        }
    }

    /// The window owning `surface`.
    pub fn term(&self, surface: &K) -> Option<TermId> {
        self.by_surface.get(surface).copied()
    }

    /// Keyboard entered `surface`: its window (if known) takes focus, which is returned.
    pub fn enter(&mut self, surface: &K) -> Option<TermId> {
        self.focus = self.term(surface);
        self.focus
    }

    /// Keyboard left `surface`: returns its window; focus clears if that window held it.
    pub fn leave(&mut self, surface: &K) -> Option<TermId> {
        let id = self.term(surface);
        if id.is_some() && self.focus == id {
            self.focus = None;
        }
        id
    }

    /// The window that receives key presses and key repeat.
    pub fn focused(&self) -> Option<TermId> {
        self.focus
    }

    /// `id` holds keyboard focus (the OSC 52 rule).
    pub fn holds_focus(&self, id: TermId) -> bool {
        self.focus == Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_route_to_their_window() {
        let mut r = Routes::default();
        r.insert(10u32, TermId(1));
        r.insert(20u32, TermId(2));
        assert_eq!(r.term(&10), Some(TermId(1)));
        assert_eq!(r.term(&20), Some(TermId(2)));
        assert_eq!(r.term(&30), None);
        // An unknown surface takes no focus.
        assert_eq!(r.enter(&30), None);
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn leave_of_another_surface_keeps_focus() {
        let mut r = Routes::default();
        r.insert(10u32, TermId(1));
        r.insert(20u32, TermId(2));
        r.enter(&20);
        // A late leave for the old surface must not drop the new focus.
        assert_eq!(r.leave(&10), Some(TermId(1)));
        assert_eq!(r.focused(), Some(TermId(2)));
        assert_eq!(r.leave(&20), Some(TermId(2)));
        assert_eq!(r.focused(), None);
    }

    /// Two-window smoke test through the routing layer: keys, repeat and OSC 52 follow focus,
    /// and closing one window leaves the other routable.
    #[test]
    fn two_windows_input_follows_focus() {
        let mut r = Routes::default();
        let (a, b) = (TermId(1), TermId(2));
        r.insert(100u32, a);
        r.insert(200u32, b);
        let mut got: Vec<(TermId, char)> = Vec::new();
        let mut key = |r: &Routes<u32>, c| {
            if let Some(t) = r.focused() {
                got.push((t, c));
            }
        };
        r.enter(&100);
        key(&r, 'x');
        assert!(r.holds_focus(a) && !r.holds_focus(b));
        r.leave(&100);
        r.enter(&200);
        key(&r, 'y'); // press
        key(&r, 'y'); // repeat follows focus
        assert!(r.holds_focus(b) && !r.holds_focus(a));
        r.remove_term(a);
        assert_eq!(r.term(&100), None);
        assert_eq!(r.term(&200), Some(b));
        key(&r, 'z');
        assert_eq!(got, vec![(a, 'x'), (b, 'y'), (b, 'y'), (b, 'z')]);
        r.remove_term(b);
        assert_eq!(r.focused(), None);
    }
}

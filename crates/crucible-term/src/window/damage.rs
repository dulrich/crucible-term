//! Buffer-age bookkeeping for painting straight into the `wl_shm` buffers (no persistent frame
//! image), after foot (`render.c` `reapply_old_damage`, `shm.c` new/resized → full repaint).
//!
//! Frames are numbered from 1 as they are committed. Each live buffer remembers the number of
//! the frame it holds. Before painting frame `n + 1` into a buffer, the buffer is brought up to
//! frame `n` ([`FrameLog::repair`]), then only this frame's damage is painted. Pure: no Wayland
//! types, so it unit-tests.

/// A damaged band: `(y, height)` in buffer pixels, full width.
pub(super) type Band = (usize, usize);

/// What must happen to a buffer before this frame's damage is painted into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Repair {
    /// The buffer already holds the last committed frame (age 0): paint only.
    Nothing,
    /// The buffer is one frame behind (age 1): copy the last frame's bands from the
    /// last-painted buffer, then paint.
    Bands(Vec<Band>),
    /// The buffer is more than one frame behind: copy the whole last-painted buffer, then paint.
    All,
    /// A new or resized buffer (or no source to copy from): repaint everything.
    Repaint,
}

/// The committed-frame counter and the last committed frame's damage.
#[derive(Debug, Default)]
pub(super) struct FrameLog {
    frame: u64,
    last: Vec<Band>,
}

impl FrameLog {
    /// The number of the last committed frame (0 before the first).
    pub(super) fn current(&self) -> u64 {
        self.frame
    }

    /// How to bring a buffer holding frame `held` (`None`: new, never painted) up to the last
    /// committed frame. `source` says whether a buffer holding that frame is still live.
    pub(super) fn repair(&self, held: Option<u64>, source: bool) -> Repair {
        match held {
            None | Some(0) => Repair::Repaint,
            Some(f) if f > self.frame => Repair::Repaint,
            Some(f) if f == self.frame => Repair::Nothing,
            _ if !source => Repair::Repaint,
            Some(f) if f + 1 == self.frame => Repair::Bands(self.last.clone()),
            Some(_) => Repair::All,
        }
    }

    /// Record a committed frame and its damage; returns its number (what the buffer now holds).
    pub(super) fn commit(&mut self, bands: Vec<Band>) -> u64 {
        self.frame += 1;
        self.last = bands;
        self.frame
    }

    /// Forget all history (resize: every buffer is dropped and new ones are fully repainted).
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_buffer_is_repainted() {
        let mut log = FrameLog::default();
        assert_eq!(log.repair(None, false), Repair::Repaint);
        log.commit(vec![(0, 100)]);
        assert_eq!(log.repair(None, true), Repair::Repaint);
    }

    #[test]
    fn age_zero_paints_only() {
        let mut log = FrameLog::default();
        let n = log.commit(vec![(0, 100)]);
        assert_eq!(log.repair(Some(n), true), Repair::Nothing);
    }

    #[test]
    fn age_one_copies_last_frame_bands() {
        let mut log = FrameLog::default();
        let a = log.commit(vec![(0, 100)]);
        log.commit(vec![(16, 16), (48, 16)]);
        assert_eq!(
            log.repair(Some(a), true),
            Repair::Bands(vec![(16, 16), (48, 16)])
        );
        // No live source holding the last frame: fall back to a full repaint.
        assert_eq!(log.repair(Some(a), false), Repair::Repaint);
    }

    #[test]
    fn age_two_copies_everything() {
        let mut log = FrameLog::default();
        let a = log.commit(vec![(0, 100)]);
        log.commit(vec![(16, 16)]);
        log.commit(vec![(32, 16)]);
        assert_eq!(log.repair(Some(a), true), Repair::All);
    }

    #[test]
    fn resize_resets_history() {
        let mut log = FrameLog::default();
        let a = log.commit(vec![(0, 100)]);
        log.reset();
        assert_eq!(log.current(), 0);
        assert_eq!(log.repair(Some(a), true), Repair::Repaint);
        assert_eq!(log.repair(None, true), Repair::Repaint);
    }

    #[test]
    fn scroll_sized_damage_is_copied_whole() {
        // A scroll damages every row: the age-1 repair copies that full-height band.
        let mut log = FrameLog::default();
        let a = log.commit(vec![(0, 2131)]);
        log.commit(vec![(0, 2131)]);
        assert_eq!(log.repair(Some(a), true), Repair::Bands(vec![(0, 2131)]));
    }
}

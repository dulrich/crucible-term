//! Debug-only diagnostics of a [`super::term::Term`]: `CTERM_DEBUG_MEM=1` (memory attribution
//! on stderr: first frame, suspend, drain, then 4 s idle) and `CTERM_DEBUG_FRAME_SUM=1` (a
//! checksum of every committed full-size buffer, and of a forced full repaint after a resume).
//! Both are off unless the variable is exactly `1`.

use super::routing::TermId;

/// The two debug switches and their running state.
#[derive(Debug)]
pub(super) struct Diag {
    /// `CTERM_DEBUG_MEM=1`.
    mem: bool,
    /// The first-frame memory line has not been printed yet.
    mem_first: bool,
    /// `CTERM_DEBUG_FRAME_SUM=1`.
    sum: bool,
    /// Log a forced-full-repaint checksum after the next commit (set on resume).
    sum_full_pending: bool,
    /// Full-size commits so far (the frame-sum sequence number; never reset).
    commits: u64,
}

impl Diag {
    /// Read both switches from the environment.
    pub(super) fn from_env() -> Diag {
        let on = |name: &str| std::env::var_os(name).is_some_and(|v| v == "1");
        Diag::new(on("CTERM_DEBUG_MEM"), on("CTERM_DEBUG_FRAME_SUM"))
    }

    fn new(mem: bool, sum: bool) -> Diag {
        Diag {
            mem,
            mem_first: true,
            sum,
            sum_full_pending: false,
            commits: 0,
        }
    }

    /// Whether memory attribution is on.
    pub(super) fn mem(&self) -> bool {
        self.mem
    }

    /// True once, on the first frame, when memory attribution is on.
    pub(super) fn first_frame_mem(&mut self) -> bool {
        self.mem && std::mem::take(&mut self.mem_first)
    }

    /// A resume forces a full repaint: checksum it after the next commit.
    pub(super) fn on_resume(&mut self) {
        self.sum_full_pending = self.sum;
    }

    /// Log the checksum of a just-painted buffer (`canvas`), and after a resume that of a full
    /// repaint (`full`, called only then). Taken before the attach, while the canvas is writable.
    pub(super) fn frame_sum(
        &mut self,
        id: TermId,
        canvas: Option<&[u8]>,
        full: impl FnOnce() -> Vec<u32>,
    ) {
        if !self.sum {
            return;
        }
        self.commits += 1;
        if let Some(bytes) = canvas {
            eprintln!(
                "crucible-term: frame-sum: {} {} {:016x}",
                id.0,
                self.commits,
                fnv1a64(bytes)
            );
        }
        if std::mem::take(&mut self.sum_full_pending) {
            eprintln!(
                "crucible-term: frame-sum-full: {} {:016x}",
                id.0,
                fnv1a64_px(&full())
            );
        }
    }
}

/// FNV-1a 64 over bytes (the `CTERM_DEBUG_FRAME_SUM` checksum).
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// [`fnv1a64`] over `Argb8888` pixels in wire (little-endian) byte order.
pub(crate) fn fnv1a64_px(px: &[u32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for p in px {
        for b in p.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_deterministic_and_matches_the_pixel_form() {
        let px = [0xff00_00ffu32, 0x1234_5678, 0];
        let bytes: Vec<u8> = px.iter().flat_map(|p| p.to_le_bytes()).collect();
        assert_eq!(fnv1a64(&bytes), fnv1a64_px(&px));
        assert_eq!(fnv1a64_px(&px), fnv1a64_px(&px));
        assert_eq!(fnv1a64(&[]), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a64_px(&px), fnv1a64_px(&[0, 0, 0]));
    }

    #[test]
    fn first_frame_mem_fires_once_and_only_when_on() {
        let mut d = Diag::new(true, false);
        assert!(d.first_frame_mem());
        assert!(!d.first_frame_mem());
        assert!(!Diag::new(false, false).first_frame_mem());
    }

    #[test]
    fn the_full_repaint_sum_is_computed_only_after_a_resume() {
        let mut d = Diag::new(false, true);
        d.frame_sum(TermId(1), None, || panic!("no resume yet"));
        d.on_resume();
        let mut called = false;
        d.frame_sum(TermId(1), None, || {
            called = true;
            vec![0]
        });
        assert!(called);
        d.frame_sum(TermId(1), None, || panic!("pending was consumed"));
        assert_eq!(d.commits, 3);
        // Off: nothing runs, nothing counts.
        let mut off = Diag::new(false, false);
        off.on_resume();
        off.frame_sum(TermId(1), None, || panic!("off"));
        assert_eq!(off.commits, 0);
    }
}

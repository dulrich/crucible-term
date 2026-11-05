//! The single outbound PTY queue. Every byte for the child (terminal replies, keys, paste) goes
//! through [`Outbox`]; the master is non-blocking, so a write that hits `WouldBlock` leaves the
//! remainder queued until the window's writable-interest source drains it. A large paste is
//! never truncated.

use std::collections::VecDeque;
use std::io::{ErrorKind, Write};

/// Bytes waiting for the child.
#[derive(Debug, Default)]
pub struct Outbox {
    buf: VecDeque<u8>,
}

impl Outbox {
    /// Queue `bytes` behind anything already waiting.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
    }

    /// Drop everything queued (after a hard write error).
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// True while bytes remain queued.
    #[cfg(test)]
    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Write as much as `w` accepts. `Ok(true)` = drained; `Ok(false)` = `WouldBlock` with the
    /// remainder kept (wait for writable); `Err` = the write failed for good.
    pub fn flush(&mut self, w: &mut impl Write) -> std::io::Result<bool> {
        while !self.buf.is_empty() {
            let (front, _) = self.buf.as_slices();
            match w.write(front) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.buf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts `cap` bytes per call and `budget` bytes in total, then reports `WouldBlock`.
    struct Slow {
        got: Vec<u8>,
        budget: usize,
    }

    impl Write for Slow {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.budget == 0 {
                return Err(ErrorKind::WouldBlock.into());
            }
            let n = b.len().min(self.budget).min(7);
            self.got.extend_from_slice(&b[..n]);
            self.budget -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn would_block_keeps_the_remainder_and_order() {
        let payload: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        let mut q = Outbox::default();
        q.push(&payload[..60_000]);
        let mut w = Slow {
            got: Vec::new(),
            budget: 1000,
        };
        assert!(!q.flush(&mut w).expect("flush"));
        assert!(q.pending());
        q.push(&payload[60_000..]); // later writes queue behind the remainder
        w.budget = usize::MAX;
        assert!(q.flush(&mut w).expect("flush"));
        assert!(!q.pending());
        assert_eq!(w.got, payload);
    }
}

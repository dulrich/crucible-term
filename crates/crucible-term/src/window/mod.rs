//! The Wayland window layer: a per-process [`host`] (one connection, seat, font pool and calloop
//! loop) owning one [`term::Term`] per xdg-toplevel (server-side decorations requested, none
//! drawn by us), `wl_shm` buffers from a `SlotPool`, frame-callback paced redraws, fractional
//! scale through `wp_fractional_scale_v1` + `wp_viewporter` (integer `wl_output` scale
//! otherwise), and each PTY master as a calloop source. Keyboard, pointer, selection and
//! clipboard are in [`input`]; surface and focus routing in [`routing`]. Standalone [`run`] is a
//! host with one window.
//!
//! With no PTY output and no blink the loop blocks in `dispatch(None)`: no timers, idle CPU ~0.

mod damage;
mod host;
#[path = "../identity.rs"]
pub(crate) mod identity;
mod input;
pub(crate) mod lifecycle;
mod routing;
mod term;

pub(crate) use host::Host;
pub use host::run;
pub(crate) use routing::TermId;

use crate::config::Settings;

/// Window and terminal settings, resolved from the config file, `-o` overrides and the CLI.
#[derive(Clone, Debug)]
pub struct Config {
    /// xdg-toplevel app id.
    pub app_id: String,
    /// Default title until the child sets one.
    pub title: String,
    /// Font, theme, scrollback, padding and cursor defaults from the config and `-o`.
    pub settings: Settings,
    /// The child's working directory.
    pub cwd: std::path::PathBuf,
    /// Initial grid size.
    pub cols: u16,
    /// Initial grid size.
    pub rows: u16,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            app_id: "crucible-term".into(),
            title: "crucible-term".into(),
            settings: Settings::default(),
            cwd: std::env::current_dir().unwrap_or_else(|_| "/".into()),
            cols: 80,
            rows: 24,
        }
    }
}

/// What can stop the window from starting.
#[derive(Debug)]
pub enum WindowError {
    /// No Wayland connection, or a required global is missing.
    Wayland(String),
    /// Font discovery failed.
    Font(crate::font::FontError),
    /// The PTY or child failed.
    Pty(crucible_term_core::pty::PtyError),
    /// The event loop failed.
    Loop(String),
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowError::Wayland(e) => write!(f, "wayland: {e}"),
            WindowError::Font(e) => write!(f, "font: {e}"),
            WindowError::Pty(e) => write!(f, "pty: {e}"),
            WindowError::Loop(e) => write!(f, "event loop: {e}"),
        }
    }
}

impl std::error::Error for WindowError {}

fn wl(e: impl std::fmt::Display) -> WindowError {
    WindowError::Wayland(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::term::exit_code;
    use crucible_term_core::pty::{Pty, SpawnSpec};
    use rustix::event::{PollFd, PollFlags, poll};
    use std::time::{Duration, Instant};

    /// The pidfd turns readable when the shell exits even though a background grandchild still
    /// holds the PTY slave (so the master sees no EOF), and the exit code is the shell's.
    #[test]
    fn pidfd_fires_while_a_grandchild_holds_the_slave() {
        let mut pty = Pty::spawn(&SpawnSpec {
            argv: vec!["sh".into(), "-c".into(), "sleep 2 & exit 7".into()],
            cwd: "/".into(),
            env: crucible_term_core::pty::inherited_env(),
            size: (80, 24),
        })
        .expect("spawn");
        let pid = rustix::process::Pid::from_raw(pty.pid() as i32).expect("pid");
        let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .expect("pidfd_open");
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let t = Instant::now();
        assert_eq!(
            poll(
                &mut fds,
                Some(&rustix::event::Timespec {
                    tv_sec: 1,
                    tv_nsec: 500_000_000,
                })
            )
            .expect("poll"),
            1
        );
        assert!(t.elapsed() < Duration::from_millis(1500));
        assert_eq!(exit_code(pty.wait().expect("wait")), 7);
    }
}

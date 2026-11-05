//! The owned PTY: open a pseudo-terminal pair, spawn a child on the slave as its controlling
//! terminal, resize it, and report the child's exit status.
//!
//! The master is opened `O_CLOEXEC`; the child gets a new session (`setsid`), the slave as its
//! controlling terminal (`TIOCSCTTY`) and stdio, and closes every other descriptor
//! (`close_range(3, ~0)`) before `exec`. `TERM=xterm-256color` is set on top of the given env.

use rustix::fs::{Mode, OFlags};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

/// What can fail around a PTY.
#[derive(Debug)]
pub enum PtyError {
    /// The argv was empty.
    EmptyArgv,
    /// A PTY system call failed (`what` names it).
    Sys {
        /// The failing step.
        what: &'static str,
        /// The OS error.
        err: std::io::Error,
    },
    /// Spawning the child failed.
    Spawn(std::io::Error),
}

impl std::fmt::Display for PtyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PtyError::EmptyArgv => write!(f, "empty argv"),
            PtyError::Sys { what, err } => write!(f, "{what}: {err}"),
            PtyError::Spawn(e) => write!(f, "spawn: {e}"),
        }
    }
}

impl std::error::Error for PtyError {}

fn sys<T>(what: &'static str, r: rustix::io::Result<T>) -> Result<T, PtyError> {
    r.map_err(|e| PtyError::Sys {
        what,
        err: e.into(),
    })
}

/// How to start the child.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    /// Program and arguments; `argv[0]` is looked up on `PATH`.
    pub argv: Vec<OsString>,
    /// Working directory.
    pub cwd: PathBuf,
    /// The child's whole environment (`TERM` is overridden).
    pub env: Vec<(OsString, OsString)>,
    /// Initial `(cols, rows)`.
    pub size: (u16, u16),
}

/// A child running on a PTY. Reading yields its output; writing feeds its input.
#[derive(Debug)]
pub struct Pty {
    master: File,
    child: Child,
}

impl Pty {
    /// Open a PTY and spawn `spec` on it.
    pub fn spawn(spec: &SpawnSpec) -> Result<Pty, PtyError> {
        let (prog, args) = spec.argv.split_first().ok_or(PtyError::EmptyArgv)?;
        let master = sys(
            "posix_openpt",
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC),
        )?;
        sys("grantpt", grantpt(&master))?;
        sys("unlockpt", unlockpt(&master))?;
        let name = sys("ptsname", ptsname(&master, Vec::new()))?;
        let slave: OwnedFd = sys(
            "open slave",
            rustix::fs::open(
                name.as_c_str(),
                OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
                Mode::empty(),
            ),
        )?;
        set_size(&master, spec.size)?;
        let stdio = |fd: &OwnedFd| -> Result<Stdio, PtyError> {
            fd.try_clone().map(Stdio::from).map_err(|e| PtyError::Sys {
                what: "dup slave",
                err: e,
            })
        };
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .env("TERM", "xterm-256color")
            .stdin(stdio(&slave)?)
            .stdout(stdio(&slave)?)
            .stderr(stdio(&slave)?);
        // SAFETY: the closure runs in the forked child before exec and only makes
        // async-signal-safe system calls (setsid, ioctl, close_range); it allocates nothing.
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()?;
                // rustix's raw fd 0, not `std::io::stdin()`: that lazily allocates its
                // buffer, which is not async-signal-safe after fork.
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
                // Ignore failure (pre-5.9 kernels): every descriptor we own is O_CLOEXEC anyway.
                libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(PtyError::Spawn)?;
        drop(slave);
        Ok(Pty {
            master: File::from(master),
            child,
        })
    }

    /// Tell the child the new `(cols, rows)` (`TIOCSWINSZ`; the kernel sends `SIGWINCH`).
    pub fn resize(&self, size: (u16, u16)) -> Result<(), PtyError> {
        set_size(&self.master, size)
    }

    /// Read child output. `Ok(0)` or an `EIO` error means the slave side closed.
    pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.master.read(buf)
    }

    /// Write input (or terminal replies) to the child.
    pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.master.write_all(bytes)
    }

    /// The master descriptor, for polling.
    pub fn master(&self) -> &File {
        &self.master
    }

    /// The child's pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The exit status if the child has exited, without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Block until the child exits.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Read output until the slave closes (EOF or `EIO`), appending to `out`.
    pub fn read_to_end(&mut self, out: &mut Vec<u8>) -> std::io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn set_size(fd: impl AsFd, (cols, rows): (u16, u16)) -> Result<(), PtyError> {
    sys(
        "TIOCSWINSZ",
        tcsetwinsize(
            fd,
            Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        ),
    )
}

/// The process environment plus nothing else — a convenience for [`SpawnSpec::env`].
pub fn inherited_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// `true` when `path` names an existing directory — a guard for [`SpawnSpec::cwd`].
pub fn is_dir(path: &Path) -> bool {
    path.is_dir()
}

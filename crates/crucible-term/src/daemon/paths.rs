//! Pure path derivation for the daemon socket, lock and log, over an injected env lookup.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where the daemon for this Wayland display listens.
#[derive(Debug, PartialEq, Eq)]
pub struct Paths {
    /// `$XDG_RUNTIME_DIR/cterm`, created `0700`.
    pub dir: PathBuf,
    /// `<dir>/<basename of WAYLAND_DISPLAY>.sock`.
    pub socket: PathBuf,
    /// `<dir>/<basename of WAYLAND_DISPLAY>.lock`.
    pub lock: PathBuf,
}

/// The socket paths, or why there are none (no runtime dir or no Wayland display).
pub fn socket_paths(env: &dyn Fn(&str) -> Option<OsString>) -> Result<Paths, String> {
    let runtime = env("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or("XDG_RUNTIME_DIR is not set")?;
    let display = env("WAYLAND_DISPLAY")
        .filter(|v| !v.is_empty())
        .ok_or("WAYLAND_DISPLAY is not set")?;
    let base = Path::new(&display)
        .file_name()
        .ok_or("WAYLAND_DISPLAY has no basename")?
        .to_os_string();
    let dir = runtime.join("cterm");
    let with = |ext: &str| {
        let mut n = base.clone();
        n.push(ext);
        dir.join(n)
    };
    Ok(Paths {
        socket: with(".sock"),
        lock: with(".lock"),
        dir,
    })
}

/// `$XDG_STATE_HOME/crucible-term/daemon.log`, fallback `~/.local/state/crucible-term/daemon.log`.
pub fn log_path(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let nonempty = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = nonempty("XDG_STATE_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| nonempty("HOME").map(|h| h.join(".local/state")))?;
    Some(base.join("crucible-term").join("daemon.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(m: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |k| m.iter().find(|(n, _)| *n == k).map(|(_, v)| v.into())
    }

    #[test]
    fn socket_lock_and_log_paths() {
        let p = socket_paths(&env(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("WAYLAND_DISPLAY", "/tmp/nested/wayland-1"),
        ]))
        .expect("paths");
        assert_eq!(p.dir, Path::new("/run/user/1000/cterm"));
        assert_eq!(p.socket, Path::new("/run/user/1000/cterm/wayland-1.sock"));
        assert_eq!(p.lock, Path::new("/run/user/1000/cterm/wayland-1.lock"));
        assert!(socket_paths(&env(&[("WAYLAND_DISPLAY", "wayland-0")])).is_err());
        assert!(socket_paths(&env(&[("XDG_RUNTIME_DIR", "/run/user/1")])).is_err());
        assert!(
            socket_paths(&env(&[
                ("XDG_RUNTIME_DIR", "rel"),
                ("WAYLAND_DISPLAY", "w")
            ]))
            .is_err()
        );
        assert_eq!(
            log_path(&env(&[("XDG_STATE_HOME", "/s"), ("HOME", "/h")])),
            Some("/s/crucible-term/daemon.log".into())
        );
        assert_eq!(
            log_path(&env(&[("HOME", "/h")])),
            Some("/h/.local/state/crucible-term/daemon.log".into())
        );
        assert_eq!(log_path(&env(&[])), None);
    }
}

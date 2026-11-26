//! Pure path derivation for the daemon socket, lock, log and session records, over an injected env lookup.

use std::ffi::OsString;
use std::path::PathBuf;

/// Where the user's daemon listens (one per user, whatever display its clients come from).
#[derive(Debug, PartialEq, Eq)]
pub struct Paths {
    /// `$XDG_RUNTIME_DIR/cterm`, created `0700`.
    pub dir: PathBuf,
    /// `<dir>/daemon.sock` (systemd's `cterm.socket` owns it when the service runs).
    pub socket: PathBuf,
    /// `<dir>/daemon.lock`, serialising client-spawned starts (`daemon-start "spawn"`).
    pub lock: PathBuf,
}

/// The socket paths, or why there are none (no absolute runtime dir). `WAYLAND_DISPLAY` plays
/// no part.
pub fn socket_paths(env: &dyn Fn(&str) -> Option<OsString>) -> Result<Paths, String> {
    let runtime = env("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or("XDG_RUNTIME_DIR is not set")?;
    let dir = runtime.join("cterm");
    Ok(Paths {
        socket: dir.join("daemon.sock"),
        lock: dir.join("daemon.lock"),
        dir,
    })
}

/// `$XDG_STATE_HOME/crucible-term/daemon.log`, fallback `~/.local/state/crucible-term/daemon.log`.
pub fn log_path(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(crate::config::xdg_home(env, "XDG_STATE_HOME", ".local/state")?.join("daemon.log"))
}

/// `$XDG_STATE_HOME/crucible-term/sessions` (fallback under `~/.local/state`): the durable session
/// records, shared by every daemon of this user (see `store`).
pub fn sessions_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(crate::config::xdg_home(env, "XDG_STATE_HOME", ".local/state")?.join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
        assert_eq!(p.socket, Path::new("/run/user/1000/cterm/daemon.sock"));
        assert_eq!(p.lock, Path::new("/run/user/1000/cterm/daemon.lock"));
        // No display at all (an ssh login): the same socket.
        assert_eq!(
            socket_paths(&env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])),
            Ok(p)
        );
        assert!(socket_paths(&env(&[("WAYLAND_DISPLAY", "wayland-0")])).is_err());
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
        assert_eq!(
            sessions_dir(&env(&[("XDG_STATE_HOME", "/s")])),
            Some("/s/crucible-term/sessions".into())
        );
    }
}

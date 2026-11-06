//! The client side of `crucible-window-identity-v1` (`protocols/crucible-window-identity-v1.xml`):
//! the generated bindings, the daemon's open decision and the granted-open step order. The
//! manager is bound only by the daemon's host; a standalone `cterm` never uses it.

/// The generated protocol code (client side).
#[allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    missing_docs,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
pub mod generated {
    use wayland_client;
    use wayland_client::protocol::*;

    /// Interface descriptors.
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocols/crucible-window-identity-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocols/crucible-window-identity-v1.xml");
}

/// What the daemon does with an `open` given its grant and the identity global.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenDecision {
    /// A grant but no global: answer `Refused{"no-identity"}` so the client runs standalone.
    Refuse,
    /// A grant and the global: open the window and bind its surface.
    OpenBound,
    /// No grant: open exactly as a standalone window.
    OpenUnbound,
}

/// The pure daemon decision: `(grant present, global present)`.
pub fn decide(grant_present: bool, global_present: bool) -> OpenDecision {
    match (grant_present, global_present) {
        (true, false) => OpenDecision::Refuse,
        (true, true) => OpenDecision::OpenBound,
        (false, _) => OpenDecision::OpenUnbound,
    }
}

/// The proxy-layer steps whose order the identity protocol pins.
pub trait OpenSteps {
    /// The spawned PTY child.
    type Pty;
    /// The `wl_surface`.
    type Surface;
    /// What the xdg role step yields.
    type Role;
    /// Step failure.
    type Err;
    /// Spawn the PTY child; returns it and its pid.
    fn spawn(&mut self) -> Result<(Self::Pty, u32), Self::Err>;
    /// Create the surface (and its viewport and fractional-scale objects).
    fn create_surface(&mut self) -> Self::Surface;
    /// Send `bind_surface(surface, grant, root_pid)`.
    fn bind_surface(&mut self, surface: &Self::Surface, grant: &str, root_pid: u32);
    /// Give the surface its xdg role.
    fn create_role(&mut self, surface: Self::Surface) -> Self::Role;
}

/// Surface and role of one open, plus the PTY when the order spawned it first.
pub type Opened<S> = (Option<<S as OpenSteps>::Pty>, <S as OpenSteps>::Role);

/// Run the surface steps in the pinned order. Granted: spawn, create_surface, bind_surface,
/// role (the child must exist for the server's parent check, and the bind must precede the
/// role). Ungranted: create_surface, role, never bind; the caller spawns afterwards, as before.
pub fn open_in_order<S: OpenSteps>(s: &mut S, grant: Option<&str>) -> Result<Opened<S>, S::Err> {
    match grant {
        Some(g) => {
            let (pty, pid) = s.spawn()?;
            let surface = s.create_surface();
            s.bind_surface(&surface, g, pid);
            Ok((Some(pty), s.create_role(surface)))
        }
        None => {
            let surface = s.create_surface();
            Ok((None, s.create_role(surface)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sha256 of the canonical protocol XML (crucible-dev-env asserts the same constant).
    const IDENTITY_XML_SHA256: &str =
        "4c28f2619fa151b526a53d44637ca6c2999803b961d7f9fc0c908f8e047cae9a";

    #[test]
    fn xml_matches_canonical_sha256() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocols/crucible-window-identity-v1.xml"
        );
        let out = std::process::Command::new("sha256sum")
            .arg(path)
            .output()
            .expect("sha256sum");
        let text = String::from_utf8(out.stdout).expect("utf8");
        assert_eq!(text.split_whitespace().next(), Some(IDENTITY_XML_SHA256));
    }

    #[test]
    fn decision_table() {
        assert_eq!(decide(true, false), OpenDecision::Refuse);
        assert_eq!(decide(true, true), OpenDecision::OpenBound);
        assert_eq!(decide(false, true), OpenDecision::OpenUnbound);
        assert_eq!(decide(false, false), OpenDecision::OpenUnbound);
    }

    #[derive(Default)]
    struct Rec(Vec<String>);
    impl OpenSteps for Rec {
        type Pty = ();
        type Surface = u8;
        type Role = ();
        type Err = ();
        fn spawn(&mut self) -> Result<((), u32), ()> {
            self.0.push("spawn".into());
            Ok(((), 42))
        }
        fn create_surface(&mut self) -> u8 {
            self.0.push("create_surface".into());
            7
        }
        fn bind_surface(&mut self, surface: &u8, grant: &str, root_pid: u32) {
            self.0.push(format!("bind {surface} {grant} {root_pid}"));
        }
        fn create_role(&mut self, _: u8) {
            self.0.push("role".into());
        }
    }

    #[test]
    fn granted_order_is_spawn_surface_bind_role() {
        let mut r = Rec::default();
        let (pty, ()) = open_in_order(&mut r, Some("g")).unwrap();
        assert!(pty.is_some());
        assert_eq!(r.0, ["spawn", "create_surface", "bind 7 g 42", "role"]);
    }

    #[test]
    fn ungranted_never_binds_and_does_not_spawn() {
        let mut r = Rec::default();
        let (pty, ()) = open_in_order(&mut r, None).unwrap();
        assert!(pty.is_none());
        assert_eq!(r.0, ["create_surface", "role"]);
    }
}

//! The real proxy layer behind the identity open order.

use super::{Wl, WpFractionalScaleV1, WpViewport};
use crate::window::identity::OpenSteps;
use crate::window::routing::TermId;
use crucible_term_core::pty::{Pty, SpawnSpec};
use smithay_client_toolkit::shell::xdg::window::WindowDecorations;
use wayland_client::Proxy;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_surface;

/// The real proxy layer behind [`open_in_order`].
pub(super) struct RealSteps<'a> {
    pub(super) wl: &'a Wl,
    pub(super) id: TermId,
    pub(super) spec: Option<SpawnSpec>,
    pub(super) viewport: Option<WpViewport>,
    pub(super) fractional: Option<WpFractionalScaleV1>,
    pub(super) surface_id: Option<ObjectId>,
}

impl OpenSteps for RealSteps<'_> {
    type Pty = Pty;
    type Surface = wl_surface::WlSurface;
    type Role = smithay_client_toolkit::shell::xdg::window::Window;
    type Err = crucible_term_core::pty::PtyError;

    fn spawn(&mut self) -> Result<(Pty, u32), Self::Err> {
        let spec = self.spec.take().expect("spawn once");
        let pty = Pty::spawn(&spec)?;
        let pid = pty.pid();
        Ok((pty, pid))
    }

    fn create_surface(&mut self) -> Self::Surface {
        let w = self.wl;
        let qh = &w.qh;
        let surface = w.compositor.create_surface(qh);
        if let (Some(vp), Some(fm)) = (&w.viewporter, &w.fractional_mgr) {
            self.viewport = Some(vp.get_viewport(&surface, qh, ()));
            self.fractional = Some(fm.get_fractional_scale(&surface, qh, self.id));
        }
        surface
    }

    fn bind_surface(&mut self, surface: &Self::Surface, grant: &str, root_pid: u32) {
        if let Some(m) = &self.wl.identity {
            m.bind_surface(surface, grant.to_owned(), root_pid, &self.wl.qh, ());
        }
    }

    fn create_role(&mut self, surface: Self::Surface) -> Self::Role {
        self.surface_id = Some(surface.id());
        self.wl
            .xdg
            .create_window(surface, WindowDecorations::RequestServer, &self.wl.qh)
    }
}

//! The seat's two selection targets, clipboard (`wl_data_device`) and primary
//! (`zwp_primary_selection`), as one generic [`Target`] each: manager, device, our current
//! source and the text it serves. sctk's types differ per protocol; the [`Kind`] trait bridges
//! them so set, paste, serve and cancel are written once. The sctk handler impls for both
//! protocols live here too.

use super::host::Host;
use crate::clipboard::{self, MIME_TYPES};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, ReadPipe, WritePipe};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::{
    PrimarySelectionDevice, PrimarySelectionDeviceHandler,
};
use smithay_client_toolkit::primary_selection::selection::{
    PrimarySelectionSource, PrimarySelectionSourceHandler,
};
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use smithay_client_toolkit::{delegate_data_device, delegate_primary_selection};
use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface;
use wayland_client::{Connection, QueueHandle};

/// One selection protocol's sctk types.
pub(super) trait Kind {
    type Mgr;
    type Dev;
    type Src;
    /// The wayland object of a source, compared against the one a handler callback names.
    type Raw: PartialEq;
    fn bind(globals: &GlobalList, qh: &QueueHandle<Host>) -> Option<Self::Mgr>;
    fn device(mgr: &Self::Mgr, qh: &QueueHandle<Host>, seat: &WlSeat) -> Self::Dev;
    fn source(mgr: &Self::Mgr, qh: &QueueHandle<Host>) -> Self::Src;
    fn publish(src: &Self::Src, dev: &Self::Dev, serial: u32);
    fn raw(src: &Self::Src) -> &Self::Raw;
    /// Open a pipe on the current selection offer. `None`: no offer or no usable MIME type;
    /// `Err`: the receive failed (the message is logged by the caller).
    fn receive(dev: &Self::Dev) -> Option<Result<ReadPipe, String>>;
}

/// The clipboard (`wl_data_device`).
pub(super) struct Clipboard;
/// The primary selection (`zwp_primary_selection`).
pub(super) struct Primary;

impl Kind for Clipboard {
    type Mgr = DataDeviceManagerState;
    type Dev = DataDevice;
    type Src = CopyPasteSource;
    type Raw = WlDataSource;
    fn bind(globals: &GlobalList, qh: &QueueHandle<Host>) -> Option<Self::Mgr> {
        DataDeviceManagerState::bind(globals, qh).ok()
    }
    fn device(mgr: &Self::Mgr, qh: &QueueHandle<Host>, seat: &WlSeat) -> Self::Dev {
        mgr.get_data_device(qh, seat)
    }
    fn source(mgr: &Self::Mgr, qh: &QueueHandle<Host>) -> Self::Src {
        mgr.create_copy_paste_source(qh, MIME_TYPES)
    }
    fn publish(src: &Self::Src, dev: &Self::Dev, serial: u32) {
        src.set_selection(dev, serial);
    }
    fn raw(src: &Self::Src) -> &Self::Raw {
        src.inner()
    }
    fn receive(dev: &Self::Dev) -> Option<Result<ReadPipe, String>> {
        let offer = dev.data().selection_offer()?;
        let mime = offer.with_mime_types(clipboard::pick_mime)?;
        Some(offer.receive(mime.to_string()).map_err(|e| e.to_string()))
    }
}

impl Kind for Primary {
    type Mgr = PrimarySelectionManagerState;
    type Dev = PrimarySelectionDevice;
    type Src = PrimarySelectionSource;
    type Raw = ZwpPrimarySelectionSourceV1;
    fn bind(globals: &GlobalList, qh: &QueueHandle<Host>) -> Option<Self::Mgr> {
        PrimarySelectionManagerState::bind(globals, qh).ok()
    }
    fn device(mgr: &Self::Mgr, qh: &QueueHandle<Host>, seat: &WlSeat) -> Self::Dev {
        mgr.get_selection_device(qh, seat)
    }
    fn source(mgr: &Self::Mgr, qh: &QueueHandle<Host>) -> Self::Src {
        mgr.create_selection_source(qh, MIME_TYPES)
    }
    fn publish(src: &Self::Src, dev: &Self::Dev, serial: u32) {
        src.set_selection(dev, serial);
    }
    fn raw(src: &Self::Src) -> &Self::Raw {
        src.inner()
    }
    fn receive(dev: &Self::Dev) -> Option<Result<ReadPipe, String>> {
        let offer = dev.data().selection_offer()?;
        let mime = offer.with_mime_types(clipboard::pick_mime)?;
        Some(offer.receive(mime.to_string()).map_err(|e| e.to_string()))
    }
}

/// One selection target's paired state.
pub(super) struct Target<K: Kind> {
    mgr: Option<K::Mgr>,
    device: Option<K::Dev>,
    source: Option<K::Src>,
    text: Arc<str>,
}

impl<K: Kind> Target<K> {
    /// No manager, no device, no source (the detached host's seat).
    pub(super) fn unbound() -> Self {
        Target {
            mgr: None,
            device: None,
            source: None,
            text: Arc::from(""),
        }
    }

    /// Bind the manager global (a missing global leaves the target unbound).
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<Host>) -> Self {
        Target {
            mgr: K::bind(globals, qh),
            ..Self::unbound()
        }
    }

    /// Create the device for `seat` once (no manager: skip).
    pub(super) fn bind_device(&mut self, qh: &QueueHandle<Host>, seat: &WlSeat) {
        if self.device.is_none() {
            self.device = self.mgr.as_ref().map(|m| K::device(m, qh, seat));
        }
    }

    /// Become the selection owner with `text`. Without manager, device or connection: nothing.
    pub(super) fn set(&mut self, qh: Option<&QueueHandle<Host>>, text: String, serial: u32) {
        let (Some(mgr), Some(dev), Some(qh)) = (&self.mgr, &self.device, qh) else {
            return;
        };
        let src = K::source(mgr, qh);
        K::publish(&src, dev, serial);
        self.text = Arc::from(text);
        self.source = Some(src);
    }

    /// The pipe to read the current selection offer from (see [`Kind::receive`]).
    pub(super) fn receive(&self) -> Option<Result<ReadPipe, String>> {
        self.device.as_ref().and_then(K::receive)
    }

    fn owns(&self, raw: &K::Raw) -> bool {
        self.source.as_ref().is_some_and(|s| K::raw(s) == raw)
    }

    /// A peer asked for our selection: answer when `raw` is our current source.
    fn send_request(&self, raw: &K::Raw, mime: &str, fd: WritePipe) {
        if self.owns(raw) && MIME_TYPES.contains(&mime) {
            serve(OwnedFd::from(fd), self.text.clone());
        }
    }

    /// The compositor replaced our selection: drop the source when it is `raw`.
    fn cancelled(&mut self, raw: &K::Raw) {
        if self.owns(raw) {
            self.source = None;
        }
    }
}

/// Answer a data request on a background thread so a slow reader cannot stall the UI.
fn serve(fd: OwnedFd, text: Arc<str>) {
    let _ = std::thread::Builder::new()
        .name("selection-write".into())
        .spawn(move || {
            let _ = File::from(fd).write_all(text.as_bytes());
        });
}

impl DataDeviceHandler for Host {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for Host {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for Host {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        self.seat_in.clipboard.send_request(source, &mime, fd);
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.seat_in.clipboard.cancelled(source);
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl PrimarySelectionDeviceHandler for Host {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl PrimarySelectionSourceHandler for Host {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime: String,
        fd: WritePipe,
    ) {
        self.seat_in.primary.send_request(source, &mime, fd);
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        self.seat_in.primary.cancelled(source);
    }
}

delegate_data_device!(Host);
delegate_primary_selection!(Host);

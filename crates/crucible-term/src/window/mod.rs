//! The Wayland window: an sctk xdg-toplevel (server-side decorations requested, none drawn by
//! us), `wl_shm` buffers from a `SlotPool`, frame-callback paced redraws, fractional scale
//! through `wp_fractional_scale_v1` + `wp_viewporter` (integer `wl_output` scale otherwise),
//! and the PTY master as a calloop source. Keyboard, pointer, selection and clipboard are in
//! [`input`]; all PTY writes go through [`App::send`] and its outbox.
//!
//! Frame pacing: [`App::request_redraw`] marks the window dirty; a frame is drawn only when
//! dirty and no frame callback is outstanding ([`App::draw`]); the `wl_surface.frame` callback
//! ([`CompositorHandler::frame`]) draws again only if new damage arrived meanwhile. With no
//! PTY output and no blink the loop blocks in `dispatch(None)`: no timers, idle CPU ~0.

mod input;

use crate::config::Settings;
use crate::font::{Fonts, Metrics};
use crate::outbox::Outbox;
use crate::render::{self, Canvas, CursorDraw, Pad, Theme};
use crate::selection::{invalidated, merge_damage};
use crucible_term_core::pty::{Pty, SpawnSpec};
use crucible_term_core::{Damage, Event, Session};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction,
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::csd_frame::WindowState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_shm,
    delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

/// Mode 2026: hold presenting at most this long while the child keeps sync set.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Cursor blink half-period.
const BLINK: Duration = Duration::from_millis(500);

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

/// Spawn `argv` on a PTY in a new window and run until the child exits or the window closes.
/// Returns the child's exit code (128 + signal when killed).
pub fn run(cfg: &Config, argv: Vec<OsString>) -> Result<i32, WindowError> {
    let conn = Connection::connect_to_env().map_err(wl)?;
    let (globals, queue) = registry_queue_init::<App>(&conn).map_err(wl)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| WindowError::Loop(e.to_string()))?;
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), queue)
        .insert(handle.clone())
        .map_err(|e| WindowError::Loop(e.to_string()))?;

    let compositor = CompositorState::bind(&globals, &qh).map_err(wl)?;
    let xdg = XdgShell::bind(&globals, &qh).map_err(wl)?;
    let shm = Shm::bind(&globals, &qh).map_err(wl)?;
    let surface = compositor.create_surface(&qh);
    let (viewport, fractional) = bind_scaling(&globals, &qh, &surface);
    let window = xdg.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title(cfg.title.clone());
    window.set_app_id(cfg.app_id.clone());
    window.commit();

    let fonts = Fonts::discover(&cfg.settings.family, cfg.settings.size_pt, 1.0)
        .map_err(WindowError::Font)?;
    let m = fonts.metrics();
    let logical = (
        u32::from(cfg.cols) * m.cell_w + 2 * cfg.settings.pad.0,
        u32::from(cfg.rows) * m.cell_h + 2 * cfg.settings.pad.1,
    );
    let cwd = cfg.cwd.clone();
    let pty = Pty::spawn(&SpawnSpec {
        argv,
        cwd,
        env: crucible_term_core::pty::inherited_env(),
        size: (cfg.cols, cfg.rows),
    })
    .map_err(WindowError::Pty)?;
    let master = pty
        .master()
        .try_clone()
        .map_err(|e| WindowError::Loop(e.to_string()))?;
    // Non-blocking so one wakeup drains everything the child wrote.
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| WindowError::Loop(e.to_string()))?;
    handle
        .insert_source(
            Generic::new(master, Interest::READ, Mode::Level),
            |_, _, app| {
                app.on_pty_readable();
                Ok(if app.exit.is_some() {
                    PostAction::Remove
                } else {
                    PostAction::Continue
                })
            },
        )
        .map_err(|e| WindowError::Loop(e.to_string()))?;

    // The child's exit also arrives on a pidfd, so a background grandchild holding the slave
    // cannot keep the window open after the shell is gone. Without pidfd the PTY EOF path
    // above still ends the window.
    match rustix::process::Pid::from_raw(pty.pid() as i32)
        .map(|p| rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()))
    {
        Some(Ok(fd)) => {
            handle
                .insert_source(
                    Generic::new(fd, Interest::READ, Mode::Level),
                    |_, _, app| {
                        app.on_child_exited();
                        Ok(PostAction::Remove)
                    },
                )
                .map_err(|e| WindowError::Loop(e.to_string()))?;
        }
        Some(Err(e)) => eprintln!("crucible-term: pidfd_open: {e}; relying on PTY EOF"),
        None => {}
    }

    let pool = SlotPool::new((logical.0 * logical.1 * 4) as usize, &shm).map_err(wl)?;
    let mut app = App {
        registry: RegistryState::new(&globals),
        seat: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),
        shm,
        pool,
        buffers: Vec::new(),
        window,
        viewport,
        has_fractional: fractional.is_some(),
        _fractional: fractional,
        qh: qh.clone(),
        handle,
        cfg: cfg.clone(),
        session: Session::new(
            usize::from(cfg.cols),
            usize::from(cfg.rows),
            cfg.settings.scrollback,
        ),
        pty,
        outbox: Outbox::default(),
        write_armed: false,
        pending_damage: None,
        inp: input::Input::new(&globals, &qh),
        fonts,
        theme: cfg.settings.theme.clone(),
        logical,
        scale: 1.0,
        int_scale: 1,
        buf: (0, 0),
        image: Vec::new(),
        configured: false,
        dirty: false,
        full: true,
        frame_pending: false,
        focused: true,
        blink_on: true,
        blink_armed: false,
        sync_since: None,
        sync_timer: false,
        last_cursor: None,
        exit: None,
        mem_debug: std::env::var_os("CTERM_DEBUG_MEM").is_some_and(|v| v == "1"),
        mem_first: true,
    };
    app.apply_cursor_defaults();
    if app.mem_debug {
        let _ =
            app.handle
                .insert_source(Timer::from_duration(Duration::from_secs(4)), |_, _, app| {
                    app.print_mem("idle-4s");
                    TimeoutAction::Drop
                });
    }
    while app.exit.is_none() {
        event_loop
            .dispatch(None, &mut app)
            .map_err(|e| WindowError::Loop(e.to_string()))?;
    }
    Ok(app.exit.unwrap_or(0))
}

fn bind_scaling(
    globals: &GlobalList,
    qh: &QueueHandle<App>,
    surface: &wl_surface::WlSurface,
) -> (Option<WpViewport>, Option<WpFractionalScaleV1>) {
    let viewporter = globals.bind::<WpViewporter, _, _>(qh, 1..=1, ()).ok();
    let manager = globals
        .bind::<WpFractionalScaleManagerV1, _, _>(qh, 1..=1, ())
        .ok();
    match (viewporter, manager) {
        (Some(vp), Some(fm)) => (
            Some(vp.get_viewport(surface, qh, ())),
            Some(fm.get_fractional_scale(surface, qh, ())),
        ),
        _ => (None, None),
    }
}

/// Window state; the calloop event loop's data.
pub struct App {
    registry: RegistryState,
    seat: SeatState,
    output: OutputState,
    shm: Shm,
    pool: SlotPool,
    buffers: Vec<Buffer>,
    window: Window,
    viewport: Option<WpViewport>,
    _fractional: Option<WpFractionalScaleV1>,
    qh: QueueHandle<App>,
    handle: LoopHandle<'static, App>,
    cfg: Config,
    session: Session,
    pty: Pty,
    /// Every byte for the child queues here (see [`App::send`]).
    outbox: Outbox,
    /// A writable-interest source is registered to drain the outbox.
    write_armed: bool,
    /// Damage taken from the core after PTY output, held until the next paint.
    pending_damage: Option<Damage>,
    inp: input::Input,
    fonts: Fonts,
    theme: Theme,
    /// Logical window size.
    logical: (u32, u32),
    /// Fractional scale (1.0 without the protocol).
    scale: f64,
    /// Integer `wl_output` scale, used only without fractional scale.
    int_scale: i32,
    has_fractional: bool,
    /// Buffer size in pixels.
    buf: (u32, u32),
    /// The persistent frame image; damage-limited paint updates it, then it is copied whole
    /// into whichever shm buffer is free (so either buffer is always fully up to date).
    image: Vec<u32>,
    configured: bool,
    dirty: bool,
    full: bool,
    frame_pending: bool,
    focused: bool,
    blink_on: bool,
    blink_armed: bool,
    /// When the current mode 2026 hold started.
    sync_since: Option<Instant>,
    sync_timer: bool,
    last_cursor: Option<usize>,
    exit: Option<i32>,
    /// `CTERM_DEBUG_MEM=1`: print memory attribution to stderr (first frame, then 4 s idle).
    mem_debug: bool,
    mem_first: bool,
}

fn exit_code(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}

impl App {
    fn effective_scale(&self) -> f64 {
        if self.has_fractional {
            self.scale
        } else {
            f64::from(self.int_scale)
        }
    }

    /// The configured logical padding in buffer pixels (sizes the grid).
    fn base_pad(&self) -> Pad {
        let s = self.effective_scale();
        let px = |v: u32| (f64::from(v) * s).round() as u32;
        Pad {
            x: px(self.cfg.settings.pad.0),
            y: px(self.cfg.settings.pad.1),
        }
    }

    /// The padding in buffer pixels: the configured logical padding scaled, plus the spare
    /// pixels split evenly when `center` is set.
    fn pad_px(&self) -> Pad {
        let base = self.base_pad();
        if self.cfg.settings.center {
            render::centered(
                self.buf.0,
                self.buf.1,
                &self.fonts.metrics(),
                self.session.size(),
                base,
            )
        } else {
            base
        }
    }

    fn on_pty_readable(&mut self) {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0;
        let history_before = self.session.history_len();
        loop {
            match self.pty.read(&mut buf) {
                Ok(0) => return self.child_gone(),
                Ok(n) => {
                    self.session.feed(&buf[..n]);
                    total += n;
                    if total >= 4 << 20 {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return self.child_gone(),
            }
        }
        let replies = self.session.take_replies();
        self.send(&replies);
        for ev in self.session.take_events() {
            match ev {
                Event::Title(t) => self.window.set_title(if t.is_empty() {
                    self.cfg.title.clone()
                } else {
                    t
                }),
                other => self.on_clipboard_event(&other),
            }
        }
        // Output over selected rows (or scrolling the history) makes the selection stale.
        let damage = self.session.take_damage();
        if let Some(sel) = &self.inp.selection
            && invalidated(
                sel,
                &damage,
                self.session.history_len(),
                self.session.history_len() != history_before,
            )
        {
            self.inp.selection = None;
            self.full = true;
        }
        self.pending_damage = Some(match self.pending_damage.take() {
            Some(p) => merge_damage(p, damage),
            None => damage,
        });
        self.blink_on = true;
        self.arm_blink();
        self.request_redraw();
    }

    /// The pidfd fired: the child has exited. Drain what the PTY still holds, then close.
    fn on_child_exited(&mut self) {
        if self.exit.is_none() {
            self.on_pty_readable();
        }
        if self.exit.is_none() {
            self.child_gone();
        }
    }

    /// Seed the session's cursor with the configured shape and blink, as if the child had sent
    /// DECSCUSR (which still overrides it later; `CSI 0 q` resets to a steady block).
    fn apply_cursor_defaults(&mut self) {
        use crucible_term_core::CursorKind;
        let s = &self.cfg.settings;
        let base = match s.cursor_shape {
            CursorKind::Underline => 3,
            CursorKind::Beam => 5,
            _ => 1,
        };
        let n = if s.cursor_blink { base } else { base + 1 };
        self.session.feed(format!("\x1b[{n} q").as_bytes());
        self.arm_blink();
    }

    fn child_gone(&mut self) {
        let code = self.pty.wait().map(exit_code).unwrap_or(1);
        self.exit = Some(code);
    }

    fn arm_blink(&mut self) {
        if self.blink_armed || !self.session.cursor_style().blinking {
            return;
        }
        self.blink_armed = true;
        let r = self
            .handle
            .insert_source(Timer::from_duration(BLINK), |_, _, app| {
                if !app.session.cursor_style().blinking || !app.focused {
                    app.blink_on = true;
                    app.blink_armed = false;
                    app.mark_cursor();
                    return TimeoutAction::Drop;
                }
                app.blink_on = !app.blink_on;
                app.mark_cursor();
                TimeoutAction::ToDuration(BLINK)
            });
        if r.is_err() {
            self.blink_armed = false;
        }
    }

    fn mark_cursor(&mut self) {
        self.request_redraw();
    }

    fn request_redraw(&mut self) {
        self.dirty = true;
        if !self.frame_pending {
            self.draw();
        }
    }

    /// Recompute buffer and grid sizes after a configure or scale change.
    fn relayout(&mut self) {
        let s = self.effective_scale();
        self.fonts.set_scale(s as f32);
        let buf = (
            ((f64::from(self.logical.0) * s).round() as u32).max(1),
            ((f64::from(self.logical.1) * s).round() as u32).max(1),
        );
        if buf != self.buf {
            self.buf = buf;
            self.image = vec![0; (buf.0 * buf.1) as usize];
            self.buffers.clear();
        }
        let m: Metrics = self.fonts.metrics();
        let (cols, rows) = render::grid_size(buf.0, buf.1, &m, self.base_pad());
        if (cols, rows) != self.session.size() {
            self.inp.selection = None;
            self.session.resize(cols, rows);
            let size = (
                u16::try_from(cols).unwrap_or(u16::MAX),
                u16::try_from(rows).unwrap_or(u16::MAX),
            );
            if let Err(e) = self.pty.resize(size) {
                eprintln!("crucible-term: pty resize: {e}");
            }
        }
        if let Some(vp) = &self.viewport {
            vp.set_destination(self.logical.0 as i32, self.logical.1 as i32);
        } else {
            self.window.wl_surface().set_buffer_scale(self.int_scale);
        }
        self.full = true;
        self.request_redraw();
    }

    /// Paint and present one frame if allowed: configured, dirty, no frame callback pending,
    /// and not inside a mode 2026 hold younger than [`SYNC_TIMEOUT`].
    fn draw(&mut self) {
        if !self.configured || !self.dirty || self.frame_pending {
            return;
        }
        if self.session.synchronized_output() {
            let since = *self.sync_since.get_or_insert_with(Instant::now);
            let left = SYNC_TIMEOUT.saturating_sub(since.elapsed());
            if !left.is_zero() {
                if !self.sync_timer {
                    self.sync_timer = true;
                    let _ = self
                        .handle
                        .insert_source(Timer::from_duration(left), |_, _, app| {
                            app.sync_timer = false;
                            app.draw();
                            TimeoutAction::Drop
                        });
                }
                return;
            }
        } else {
            self.sync_since = None;
        }
        self.dirty = false;
        let damage = merge_damage(
            self.pending_damage
                .take()
                .unwrap_or(Damage::Rows(Vec::new())),
            self.session.take_damage(),
        );
        // Scrolled back: damage rows are screen rows, not viewport rows, so repaint everything.
        let scrolled = self.session.display_offset() > 0;
        let grid = self.session.viewport();
        let rows = if self.full || scrolled {
            None
        } else {
            render::rows_to_paint(&damage, self.last_cursor, grid.cursor.1)
        };
        self.full = false;
        self.last_cursor = Some(grid.cursor.1);
        // Fallback fonts for new text: the only I/O around a frame, kept outside `paint`.
        for r in rows.clone().unwrap_or_else(|| (0..grid.rows).collect()) {
            if let Some(row) = grid.screen.get(r) {
                for c in &row.cells {
                    self.fonts.prepare(&c.text);
                }
            }
        }
        let mut style = self.session.cursor_style();
        if scrolled {
            style.shape = crucible_term_core::CursorKind::Hidden;
        }
        let cursor = CursorDraw {
            style,
            focused: self.focused,
            blink_on: self.blink_on,
        };
        let (w, h) = (self.buf.0 as usize, self.buf.1 as usize);
        let pad = self.pad_px();
        let top = self.session.viewport_top();
        let sel = self.inp.selection.as_ref();
        let selected = |r: usize, c: usize| sel.is_some_and(|s| s.contains(top + r, c));
        let bands = render::paint(
            &mut Canvas {
                px: &mut self.image,
                width: w,
                height: h,
            },
            &grid,
            &self.theme,
            &mut self.fonts,
            pad,
            cursor,
            rows.as_deref(),
            sel.is_some().then_some(&selected as render::Selected<'_>),
        );
        if bands.is_empty() {
            return;
        }
        let Some(buffer) = self.free_buffer() else {
            eprintln!("crucible-term: no shm buffer available");
            self.dirty = true;
            return;
        };
        if let Some(canvas) = self.pool.canvas(&buffer) {
            for (dst, src) in canvas.chunks_exact_mut(4).zip(&self.image) {
                dst.copy_from_slice(&src.to_le_bytes());
            }
        }
        let surface = self.window.wl_surface().clone();
        for (y, bh) in bands {
            surface.damage_buffer(0, y as i32, w as i32, bh as i32);
        }
        surface.frame(&self.qh, surface.clone());
        if let Err(e) = buffer.attach_to(&surface) {
            eprintln!("crucible-term: attach: {e}");
        }
        self.window.commit();
        self.frame_pending = true;
        self.buffers.push(buffer);
        if self.buffers.len() > 2 {
            self.buffers.remove(0);
        }
        if self.mem_debug && self.mem_first {
            self.mem_first = false;
            self.print_mem("first-frame");
        }
    }

    /// Debug-only (`CTERM_DEBUG_MEM=1`): byte sizes of the big per-window and shareable pieces.
    fn print_mem(&self, when: &str) {
        let (faces, emoji, nfaces, nshapes, nglyphs, gbytes) = self.fonts.mem_stats();
        let (cols, rows) = self.session.size();
        eprintln!(
            "crucible-term: mem: {when} image_bytes={} buf={}x{} pool_bytes={} live_buffers={}",
            self.image.len() * 4,
            self.buf.0,
            self.buf.1,
            self.pool.len(),
            self.buffers.len()
        );
        eprintln!(
            "crucible-term: mem: {when} face_bytes={faces} emoji_face_bytes={emoji} faces={nfaces} \
             shape_entries={nshapes} glyph_entries={nglyphs} glyph_bitmap_bytes={gbytes}"
        );
        eprintln!(
            "crucible-term: mem: {when} session_grid_bytes_est={} cols={cols} rows={rows} history_rows={}",
            self.session.grid_bytes_estimate(),
            self.session.history_len()
        );
    }

    /// A buffer the compositor has released, or a new one (double buffering: at most two live).
    fn free_buffer(&mut self) -> Option<Buffer> {
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| self.pool.canvas(b).is_some())
        {
            return Some(self.buffers.remove(i));
        }
        let (w, h) = (self.buf.0 as i32, self.buf.1 as i32);
        self.pool
            .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
            .map(|(b, _)| b)
            .map_err(|e| eprintln!("crucible-term: create buffer: {e}"))
            .ok()
    }

    fn close(&mut self) {
        let pid = rustix::process::Pid::from_raw(self.pty.pid() as i32);
        if let Some(pid) = pid {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::HUP);
        }
        // Give the child up to 500 ms to exit on SIGHUP; never block the close on it.
        let deadline = Instant::now() + Duration::from_millis(500);
        let code = loop {
            match self.pty.try_wait() {
                Ok(Some(s)) => break exit_code(s),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break 128 + 1,
            }
        };
        self.exit = Some(code);
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if !self.has_fractional && factor != self.int_scale {
            self.int_scale = factor.max(1);
            self.relayout();
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        self.draw();
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.close();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        if let (Some(w), Some(h)) = configure.new_size {
            self.logical = (w.get(), h.get());
        }
        let focused = configure.state.contains(WindowState::ACTIVATED);
        if focused != self.focused {
            self.focused = focused;
            self.arm_blink();
        }
        self.configured = true;
        self.relayout();
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.inp.bind_devices(qh, &seat);
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        match cap {
            Capability::Keyboard if self.inp.keyboard.is_none() => {
                // xkbcommon keymap from the compositor; sctk's calloop timer drives repeat
                // from `repeat_info` (no busy loop, no timer while no key is held).
                let kbd = self.seat.get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.handle.clone(),
                    Box::new(|app: &mut App, _, event| app.on_key_repeat(&event)),
                );
                match kbd {
                    Ok(k) => self.inp.keyboard = Some(k),
                    Err(e) => eprintln!("crucible-term: keyboard: {e}"),
                }
            }
            Capability::Pointer if self.inp.pointer.is_none() => {
                match self.seat.get_pointer(qh, &seat) {
                    Ok(p) => self.inp.pointer = Some(p),
                    Err(e) => eprintln!("crucible-term: pointer: {e}"),
                }
            }
            _ => {}
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        cap: Capability,
    ) {
        match cap {
            Capability::Keyboard => {
                if let Some(k) = self.inp.keyboard.take()
                    && k.version() >= 3
                {
                    k.release();
                }
            }
            Capability::Pointer => {
                if let Some(p) = self.inp.pointer.take()
                    && p.version() >= 3
                {
                    p.release();
                }
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<WpViewporter, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let s = f64::from(scale) / 120.0;
            if (s - app.scale).abs() > f64::EPSILON {
                app.scale = s;
                app.relayout();
            }
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::event::{PollFd, PollFlags, poll};

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

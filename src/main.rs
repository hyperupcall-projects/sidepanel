//! sidepanel — a low-latency side-panel launcher for KDE Plasma on Wayland.
//!
//! Design:
//!   * a single long-running process (never spawn-per-press),
//!   * a global shortcut registered through KGlobalAccel (see `hotkey`),
//!   * a `zwlr_layer_shell_v1` surface anchored to the left or right edge,
//!   * the D-Bus listener and the Wayland loop are multiplexed through calloop.
//!
//! Wayland only. There is deliberately no X11 path.

mod config;
mod hotkey;
mod panel;

use std::num::NonZeroU32;

use anyhow::{anyhow, Context, Result};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::config::{Config, Side};
use crate::hotkey::HotkeyEvent;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = Config::load();
    log::info!(
        "sidepanel starting (side={:?}, width={}px, shortcut={})",
        config.side,
        config.width,
        config.shortcut
    );

    let conn = Connection::connect_to_env()
        .context("could not connect to a Wayland session (is WAYLAND_DISPLAY set?)")?;
    let (globals, event_queue) =
        registry_queue_init::<App>(&conn).context("failed to enumerate Wayland globals")?;
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor is not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("zwlr_layer_shell_v1 is not available (KWin should provide it)")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm is not available")?;
    let output_state = OutputState::new(&globals, &qh);
    let pool = SlotPool::new(config.width as usize * 2160 * 4, &shm)
        .context("failed to create the shared-memory pool")?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state,
        shm,
        compositor_state,
        layer_shell,
        pool,
        qh: qh.clone(),
        config,
        panel: None,
    };

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("failed to create the calloop event loop")?;
    let handle = event_loop.handle();

    // Wayland events. `insert` also dispatches pending events for us.
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .context("failed to insert the Wayland event source")?;

    // Hotkey events, forwarded from the KGlobalAccel thread.
    let (tx, channel) = calloop::channel::channel::<HotkeyEvent>();
    handle
        .insert_source(channel, |event, _, app| {
            if let calloop::channel::Event::Msg(HotkeyEvent::Toggle) = event {
                app.toggle();
            }
        })
        .map_err(|err| anyhow!("failed to insert the hotkey event source: {err:?}"))?;

    hotkey::spawn(tx, app.config.clone()).context("failed to start the global-shortcut listener")?;

    event_loop
        .run(None, &mut app, |_| {})
        .context("the event loop terminated with an error")?;
    Ok(())
}

/// A currently-mapped panel surface.
struct Panel {
    layer: LayerSurface,
    width: u32,
    height: u32,
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    pool: SlotPool,
    qh: QueueHandle<App>,
    config: Config,
    panel: Option<Panel>,
}

impl App {
    fn toggle(&mut self) {
        if self.panel.is_some() {
            self.hide();
        } else {
            self.show();
        }
    }

    /// Create and map the layer surface. The compositor replies with a
    /// `configure` event, at which point we draw the first frame.
    fn show(&mut self) {
        let qh = self.qh.clone();
        let surface = self.compositor_state.create_surface(&qh);
        let layer =
            self.layer_shell
                .create_layer_surface(&qh, surface, Layer::Top, Some("sidepanel"), None);

        layer.set_size(self.config.width, 0);
        layer.set_anchor(match self.config.side {
            Side::Left => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
            Side::Right => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
        });
        // Overlay the desktop rather than reserving space for a panel.
        layer.set_exclusive_zone(0);
        // Grab the keyboard while shown, so typing goes to the launcher.
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_margin(0, 0, 0, 0);
        layer.commit();

        self.panel = Some(Panel {
            layer,
            width: self.config.width,
            height: 0,
        });
        log::debug!("panel shown");
    }

    /// Dropping the `LayerSurface` destroys the underlying `wl_surface`, which
    /// unmaps it.
    fn hide(&mut self) {
        if self.panel.take().is_some() {
            log::debug!("panel hidden");
        }
    }

    fn draw(&mut self) {
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        if panel.height == 0 {
            return;
        }

        let width = panel.width.max(1);
        let height = panel.height.max(1);
        let stride = (width * 4) as i32;

        let (buffer, canvas) = self
            .pool
            .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
            .expect("failed to create a shared-memory buffer");

        panel::draw(canvas, width, height, self.config.side);

        let surface = panel.layer.wl_surface();
        surface.damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(surface).expect("failed to attach the buffer");
        panel.layer.commit();
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // The panel is static, so we never request frame callbacks.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.panel.as_ref().map(|panel| &panel.layer) == Some(layer) {
            self.panel = None;
            log::debug!("panel closed by the compositor");
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let width = NonZeroU32::new(configure.new_size.0).map_or(self.config.width, NonZeroU32::get);
        let height = NonZeroU32::new(configure.new_size.1).map_or(0, NonZeroU32::get);

        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        if &panel.layer != layer {
            return;
        }
        panel.width = width;
        panel.height = height;
        self.draw();
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(App);

//! sidepanel — a low-latency side-panel launcher for KDE Plasma on Wayland.
//!
//! Design:
//!   * a single long-running process (never spawn-per-press),
//!   * a global shortcut registered through KGlobalAccel (see `hotkey`),
//!   * a transparent layer surface containing four edges and four corners,
//!   * the D-Bus listener and the Wayland loop are multiplexed through calloop.
//!
//! Wayland only. There is deliberately no X11 path.

mod animation;
mod browser;
mod clipboard;
mod config;
mod editor;
mod favicon;
mod frame;
mod hotkey;
mod panel;
mod text;
mod theme;

use std::num::NonZeroU32;

use anyhow::{anyhow, Context, Result};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
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
    protocol::{wl_keyboard, wl_output, wl_pointer::WlPointer, wl_shm, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};

use wayland_protocols::xdg::foreign::zv2::client::{
    zxdg_exported_v2::{self, ZxdgExportedV2},
    zxdg_exporter_v2::ZxdgExporterV2,
};

use crate::config::Config;
use crate::hotkey::HotkeyEvent;
use crate::panel::Action;

/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Browser-helper mode: runs the pre-warmed window, driven over stdin.
    // Optional arguments: the initial window size (width height).
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(browser::ARG) {
        let width = args.next().and_then(|arg| arg.parse().ok());
        let height = args.next().and_then(|arg| arg.parse().ok());
        return browser::run(width, height);
    }

    let config = Config::load();
    log::info!(
        "sidepanel starting (edges={} / {} / {} / {}px, shortcut={})",
        config.left_width,
        config.right_width,
        config.top_height,
        config.bottom_height,
        config.shortcut
    );

    // Pre-warm the browser helper process (hidden window) and load the
    // panel font up front.
    let browser = browser::Browser::start(config.browser_width, config.browser_height);
    let font = text::TextRenderer::load();
    let labels: Vec<String> = config.websites().map(|site| site.label()).collect();
    let favicon_targets: Vec<String> = config.websites().map(|site| site.url.clone()).collect();
    let icons: Vec<Option<favicon::Icon>> = (0..labels.len()).map(|_| None).collect();

    let conn = Connection::connect_to_env()
        .context("could not connect to a Wayland session (is WAYLAND_DISPLAY set?)")?;
    let (globals, event_queue) =
        registry_queue_init::<App>(&conn).context("failed to enumerate Wayland globals")?;
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor is not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("zwlr_layer_shell_v1 is not available (KWin should provide it)")?;
    let exporter = globals.bind::<ZxdgExporterV2, _, _>(&qh, 1..=1, ()).ok();
    if exporter.is_none() {
        log::warn!(
            "xdg-foreign unavailable; desktop dimming disabled to keep the browser unobscured"
        );
    }
    let shm = Shm::bind(&globals, &qh).context("wl_shm is not available")?;
    let output_state = OutputState::new(&globals, &qh);
    let pool =
        SlotPool::new(1920 * 1080 * 4, &shm).context("failed to create the shared-memory pool")?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state,
        seat_state: SeatState::new(&globals, &qh),
        shm,
        compositor_state,
        layer_shell,
        pool,
        qh: qh.clone(),
        active_shortcut: config.shortcut.clone(),
        config,
        panel: None,
        backdrop: None,
        backdrop_export: None,
        exporter,
        settings: None,
        system_dark: false,
        pointer: None,
        keyboard: None,
        modifiers: Modifiers::default(),
        shortcut_requests: None,
        ui: panel::Ui::default(),
        labels,
        icons,
        icon_urls: favicon_targets.clone(),
        favicon_sender: None,
        clipboard_sender: None,
        editor_revision: 0,
        font,
        browser,
    };

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("failed to create the calloop event loop")?;
    let handle = event_loop.handle();

    // Wayland events. `insert` also dispatches pending events for us.
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .context("failed to insert the Wayland event source")?;

    // Favicon results, fetched in the background.
    let (fav_tx, fav_channel) = calloop::channel::channel::<favicon::Event>();
    handle
        .insert_source(fav_channel, |event, _, app| {
            if let calloop::channel::Event::Msg(event) = event {
                app.on_favicon(event);
            }
        })
        .map_err(|err| anyhow!("failed to insert the favicon event source: {err:?}"))?;
    app.favicon_sender = Some(fav_tx.clone());
    favicon::spawn(favicon_targets, fav_tx);

    let (clipboard_tx, clipboard_channel) = calloop::channel::channel::<clipboard::Event>();
    handle
        .insert_source(clipboard_channel, |event, _, app| {
            if let calloop::channel::Event::Msg(event) = event {
                if app.ui.settings_open
                    && app.ui.contents_tab
                    && app.editor_revision == event.revision
                    && app.ui.editor.focus == Some(event.field)
                {
                    match event.text {
                        Ok(text) => app.ui.editor.input_mut(event.field).insert(&text),
                        Err(message) => app.ui.editor.message = message,
                    }
                    app.draw();
                }
            }
        })
        .map_err(|err| anyhow!("failed to insert clipboard event source: {err:?}"))?;
    app.clipboard_sender = Some(clipboard_tx);

    let (theme_tx, theme_channel) = calloop::channel::channel::<bool>();
    handle
        .insert_source(theme_channel, |event, _, app| {
            if let calloop::channel::Event::Msg(dark) = event {
                app.system_dark = dark;
                if app.config.theme == config::Theme::System {
                    app.draw();
                }
            }
        })
        .map_err(|err| anyhow!("failed to insert appearance event source: {err:?}"))?;
    theme::spawn(theme_tx);

    // Hotkey events, forwarded from the KGlobalAccel thread.
    let (tx, channel) = calloop::channel::channel::<HotkeyEvent>();
    handle
        .insert_source(channel, |event, _, app| {
            if let calloop::channel::Event::Msg(event) = event {
                match event {
                    HotkeyEvent::Toggle => {
                        if !app.ui.recording_shortcut && !app.ui.shortcut_pending {
                            app.toggle();
                        }
                    }
                    HotkeyEvent::Registered(shortcut) => {
                        app.active_shortcut = shortcut;
                        app.draw();
                    }
                    HotkeyEvent::Rebound(result) => {
                        app.ui.shortcut_pending = false;
                        match result {
                            Ok(shortcut) => {
                                app.active_shortcut = shortcut.clone();
                                app.config.shortcut = shortcut;
                                app.ui.shortcut_message = match app.config.save() {
                                    Ok(()) => "Shortcut saved".into(),
                                    Err(err) => {
                                        log::warn!("could not save shortcut: {err:#}");
                                        "Applied; config could not be saved".into()
                                    }
                                };
                            }
                            Err(err) => {
                                log::warn!("shortcut change failed: {err}");
                                app.ui.shortcut_message = err;
                            }
                        }
                        app.draw();
                    }
                }
            }
        })
        .map_err(|err| anyhow!("failed to insert the hotkey event source: {err:?}"))?;

    app.shortcut_requests = Some(
        hotkey::spawn(tx, app.config.clone())
            .context("failed to start the global-shortcut listener")?,
    );

    event_loop
        .run(None, &mut app, |_| {})
        .context("the event loop terminated with an error")?;
    Ok(())
}

enum SurfaceKind {
    Backdrop,
    Frame,
    Settings,
}

/// A currently-mapped panel surface.
struct Panel {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    opening: animation::Opening,
    frame_pending: bool,
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    pool: SlotPool,
    qh: QueueHandle<App>,
    config: Config,
    active_shortcut: String,
    panel: Option<Panel>,
    backdrop: Option<Panel>,
    backdrop_export: Option<ZxdgExportedV2>,
    exporter: Option<ZxdgExporterV2>,
    settings: Option<Panel>,
    system_dark: bool,
    /// Pointer object; created when the seat advertises the capability.
    pointer: Option<WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    shortcut_requests: Option<std::sync::mpsc::Sender<String>>,
    /// Drop-down / hover state of the panel content.
    ui: panel::Ui,
    /// Row labels, parallel to the flattened section websites.
    labels: Vec<String>,
    /// Favicons as they arrive from the background fetch.
    icons: Vec<Option<favicon::Icon>>,
    icon_urls: Vec<String>,
    favicon_sender: Option<calloop::channel::Sender<favicon::Event>>,
    clipboard_sender: Option<calloop::channel::Sender<clipboard::Event>>,
    editor_revision: u64,
    /// System font for panel text (absent = draw without text).
    font: Option<text::TextRenderer>,
    /// Pre-warmed browser helper process.
    browser: browser::Browser,
}

impl App {
    fn toggle(&mut self) {
        // Meta+F also dismisses any browser window this panel opened — on
        // both directions of the toggle (summoning the panel starts clean,
        // hiding it takes the browser with it).
        self.browser.dismiss();
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
        let layer = self.layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some(self.config.panel_animation.namespace("sidepanel")),
            None,
        );

        // One output-sized transparent surface gives all eight pieces the
        // same output and atomic geometry. Only the cards accept pointer input.
        layer.set_size(0, 0);
        layer.set_anchor(Anchor::LEFT | Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM);
        // Overlay the desktop rather than reserving space for a panel.
        layer.set_exclusive_zone(0);
        // Grab the keyboard while shown, so typing goes to the launcher.
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_margin(0, 0, 0, 0);
        layer.commit();

        self.panel = Some(Panel {
            layer,
            width: 0,
            height: 0,
            configured: false,
            opening: animation::Opening::new(
                self.config.panel_animation,
                self.config.fade_duration_ms,
            ),
            frame_pending: false,
        });
        if self.exporter.is_some() {
            self.browser.wait_for_parent();
        }
        self.ui.select_first(self.labels.len());
        log::debug!("panel shown");
    }

    /// Dropping the `LayerSurface` destroys the underlying `wl_surface`, which
    /// unmaps it.
    fn hide(&mut self) {
        self.editor_revision = self.editor_revision.wrapping_add(1);
        self.ui.editor.focus = None;
        self.browser.dismiss();
        self.browser.clear_parent();
        if let Some(exported) = self.backdrop_export.take() {
            exported.destroy();
        }
        self.backdrop = None;
        self.ui.recording_shortcut = false;
        self.settings = None;
        self.ui.settings_open = false;
        self.ui.hover = None;
        if self.panel.take().is_some() {
            log::debug!("panel hidden");
        }
    }

    /// Map on the frame's actual output. Exporting this top-level equivalent
    /// surface lets KWin constrain our ordinary GTK browser above the backdrop.
    /// The overlay layer containing the cards stays above both.
    fn show_backdrop(&mut self, output: &wl_output::WlOutput) {
        let Some(exporter) = &self.exporter else {
            return;
        };
        if self.backdrop.is_some() {
            return;
        }
        let Some(frame) = &self.panel else {
            return;
        };
        // The backdrop joins the frame's timeline, even if its configure is late.
        let opening = frame.opening.clone();
        let surface = self.compositor_state.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Top,
            Some(opening.mode.namespace("sidepanel-backdrop")),
            Some(output),
        );
        layer.set_size(0, 0);
        layer.set_anchor(Anchor::LEFT | Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let region =
            Region::new(&self.compositor_state).expect("could not create empty input region");
        layer
            .wl_surface()
            .set_input_region(Some(region.wl_region()));
        self.backdrop_export = Some(exporter.export_toplevel(layer.wl_surface(), &self.qh, ()));
        layer.commit();
        self.backdrop = Some(Panel {
            layer,
            width: 0,
            height: 0,
            configured: false,
            opening,
            frame_pending: false,
        });
    }

    fn dimensions(&self) -> [u32; 4] {
        [
            self.config.left_width,
            self.config.right_width,
            self.config.top_height,
            self.config.bottom_height,
        ]
    }

    /// Place settings inward from the bottom-left box, fitting the entire
    /// card on the same output even when the requested edge sizes are large.
    fn settings_placement(&self) -> Option<(u32, u32, i32, i32)> {
        let panel = self.panel.as_ref()?;
        let corner = frame::layout(
            panel.width,
            panel.height,
            self.dimensions(),
            self.config.margin,
        )
        .into_iter()
        .find(|r| r.part == frame::Part::BottomLeft)?;
        let gap = corner.x;
        let left = corner.x + corner.w + gap;
        let bottom = panel.height - corner.y + gap;
        let available_w = panel.width.saturating_sub(left + gap).max(1);
        let available_h = panel.height.saturating_sub(bottom + gap).max(1);
        let scale = (available_w as f64 / panel::SETTINGS_WIDTH as f64)
            .min(available_h as f64 / panel::SETTINGS_HEIGHT as f64)
            .min(1.0);
        Some((
            (panel::SETTINGS_WIDTH as f64 * scale).floor().max(1.0) as u32,
            (panel::SETTINGS_HEIGHT as f64 * scale).floor().max(1.0) as u32,
            bottom as i32,
            left as i32,
        ))
    }

    fn show_settings(&mut self) {
        let Some((width, height, bottom, left)) = self.settings_placement() else {
            self.ui.settings_open = false;
            return;
        };
        let panel = self.panel.as_ref().unwrap();
        let surface = self.compositor_state.create_surface(&self.qh);
        let output = panel
            .layer
            .wl_surface()
            .data::<smithay_client_toolkit::compositor::SurfaceData<()>>()
            .and_then(|data| data.outputs().next());
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some(self.config.panel_animation.namespace("sidepanel-settings")),
            output.as_ref(),
        );
        layer.set_size(width, height);
        layer.set_anchor(Anchor::LEFT | Anchor::BOTTOM);
        layer.set_margin(0, 0, bottom, left);
        layer.set_exclusive_zone(0);
        // Keyboard focus stays on the frame, including while recording shortcuts.
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.settings = Some(Panel {
            layer,
            width,
            height,
            configured: false,
            opening: animation::Opening::new(
                self.config.panel_animation,
                self.config.fade_duration_ms,
            ),
            frame_pending: false,
        });
    }

    fn update_settings_geometry(&self) {
        if let (Some(settings), Some((width, height, bottom, left))) =
            (&self.settings, self.settings_placement())
        {
            settings.layer.set_size(width, height);
            settings.layer.set_margin(0, 0, bottom, left);
            settings.layer.commit();
        }
    }

    fn draw(&mut self) {
        self.draw_surfaces(None);
    }

    fn draw_surfaces(&mut self, only: Option<&wl_surface::WlSurface>) {
        let content = panel::Content {
            config: &self.config,
            labels: &self.labels,
            icons: &self.icons,
            font: self.font.as_ref(),
            browser_width: self.config.browser_width,
            browser_height: self.config.browser_height,
            theme: self.config.theme,
            shortcut: &self.active_shortcut,
            palette: panel::Palette::new(self.config.theme.is_dark(self.system_dark)),
            margin: self.config.margin,
            border_radius: self.config.border_radius,
            opacity: self.config.opacity,
            desktop_dim: self.config.desktop_dim,
            panel_animation: self.config.panel_animation,
            fade_duration_ms: self.config.fade_duration_ms,
            dimensions: self.dimensions(),
        };
        let now = std::time::Instant::now();
        for (surface, kind) in [
            (&mut self.backdrop, SurfaceKind::Backdrop),
            (&mut self.panel, SurfaceKind::Frame),
            (&mut self.settings, SurfaceKind::Settings),
        ] {
            let Some(panel) = surface else {
                continue;
            };
            if !panel.configured || panel.width == 0 || panel.height == 0 || panel.frame_pending {
                continue;
            }
            if only.is_some_and(|surface| surface != panel.layer.wl_surface()) {
                continue;
            }
            let width = panel.width.max(1);
            let height = panel.height.max(1);
            let stride = (width * 4) as i32;
            let (buffer, canvas) = self
                .pool
                .create_buffer(
                    width as i32,
                    height as i32,
                    stride,
                    wl_shm::Format::Argb8888,
                )
                .expect("failed to create a shared-memory buffer");
            match kind {
                SurfaceKind::Backdrop => panel::draw_backdrop(canvas, content.desktop_dim),
                SurfaceKind::Settings => {
                    panel::draw_settings(canvas, width, height, &self.ui, &content)
                }
                SurfaceKind::Frame => {
                    panel::draw_frame(canvas, width, height, &self.ui, &content);
                    // Let the launched browser take keyboard focus. Clicking a card
                    // can focus the launcher again; shortcut recording stays exclusive.
                    panel.layer.set_keyboard_interactivity(
                        if self.browser.is_open()
                            && !self.ui.recording_shortcut
                            && !self.ui.settings_open
                        {
                            KeyboardInteractivity::OnDemand
                        } else {
                            KeyboardInteractivity::Exclusive
                        },
                    );
                    let region =
                        Region::new(&self.compositor_state).expect("could not create input region");
                    for rect in frame::layout(width, height, content.dimensions, content.margin) {
                        region.add(rect.x as i32, rect.y as i32, rect.w as i32, rect.h as i32);
                    }
                    panel
                        .layer
                        .wl_surface()
                        .set_input_region(Some(region.wl_region()));
                }
            }
            let (opacity, animating) = panel.opening.sample(now);
            animation::apply_opacity(canvas, opacity);
            let surface = panel.layer.wl_surface();
            surface.damage_buffer(0, 0, width as i32, height as i32);
            buffer
                .attach_to(surface)
                .expect("failed to attach the buffer");
            if animating {
                surface.frame(&self.qh, FrameCallbackData(surface.clone()));
                panel.frame_pending = true;
            }
            panel.layer.commit();
        }
    }

    /// Handle launcher keys, leaving shortcut recording to its own handler.
    fn launcher_key(&mut self, key: Keysym, repeat: bool) {
        if self.ui.recording_shortcut || self.ui.shortcut_pending {
            return;
        }
        let Some(panel) = &self.panel else {
            return;
        };
        let hits = panel::frame_layout(
            panel.width,
            panel.height,
            self.dimensions(),
            self.config.margin,
            &self.ui,
            &self.config.sections,
        );
        if self.modifiers.alt
            && !self.modifiers.ctrl
            && !self.modifiers.logo
            && !self.modifiers.shift
            && matches!(key, Keysym::j | Keysym::J | Keysym::k | Keysym::K)
        {
            self.ui
                .move_selection(&hits, matches!(key, Keysym::j | Keysym::J));
            self.draw();
        } else if !repeat
            && !self.modifiers.alt
            && !self.modifiers.ctrl
            && !self.modifiers.logo
            && !self.modifiers.shift
            && matches!(key, Keysym::Return | Keysym::KP_Enter)
        {
            if let Some(action) = self.ui.selected_action(&hits) {
                self.activate(action);
            }
        }
    }

    /// Act on a click or keyboard activation; returns whether a redraw is needed.
    fn activate(&mut self, action: Action) -> bool {
        match action {
            Action::ToggleSection(index) => {
                if self.ui.collapsed.contains(&index) {
                    self.ui.collapsed.retain(|&i| i != index);
                } else {
                    self.ui.collapsed.push(index);
                }
                true
            }
            Action::ScrollRectangle(rectangle, delta) => {
                if let Some(surface) = &self.panel {
                    let part = match rectangle {
                        config::Rectangle::Left => frame::Part::Left,
                        config::Rectangle::Right => frame::Part::Right,
                        config::Rectangle::Top => frame::Part::Top,
                        config::Rectangle::Bottom => frame::Part::Bottom,
                    };
                    if let Some(rect) = frame::layout(
                        surface.width,
                        surface.height,
                        self.dimensions(),
                        self.config.margin,
                    )
                    .iter()
                    .find(|r| r.part == part)
                    {
                        panel::scroll_rectangle(
                            rect.w,
                            rect.h,
                            &mut self.ui,
                            &self.config.sections,
                            rectangle,
                            delta,
                        );
                    }
                }
                true
            }
            Action::Open(index) => {
                let Some(site) = self.config.websites().nth(index) else {
                    return false;
                };
                let title = self
                    .labels
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| site.url.clone());
                log::info!("panel: opening {}", site.url);
                self.browser.open(&site.url, &title);
                true
            }
            Action::ToggleSettings => {
                self.editor_revision = self.editor_revision.wrapping_add(1);
                self.ui.hover = None;
                self.ui.recording_shortcut = false;
                self.ui.editor.focus = None;
                if self.settings.take().is_some() {
                    self.ui.settings_open = false;
                } else {
                    self.ui.settings_open = true;
                    let rectangle = self.ui.editor.rectangle;
                    self.ui.editor.select_rectangle(rectangle, &self.config);
                    self.show_settings();
                }
                true
            }
            Action::RecordShortcut => {
                if self.ui.shortcut_pending {
                    return false;
                }
                if self.keyboard.is_none() {
                    self.ui.shortcut_message = "Keyboard unavailable".into();
                } else {
                    self.ui.recording_shortcut = !self.ui.recording_shortcut;
                    self.ui.shortcut_message = if self.ui.recording_shortcut {
                        "Use Ctrl, Alt or Meta with a key".into()
                    } else {
                        "Shortcut change cancelled".into()
                    };
                }
                true
            }
            Action::SetTheme(theme) => {
                self.config.theme = theme;
                self.save_config();
                true
            }
            Action::SetPanelAnimation(mode) => {
                self.config.panel_animation = mode;
                self.save_config();
                true
            }
            Action::BumpFadeDuration(delta) => {
                let next = (i64::from(self.config.fade_duration_ms) + i64::from(delta))
                    .clamp(0, i64::from(config::MAX_FADE_DURATION_MS))
                    as u32;
                if next == self.config.fade_duration_ms {
                    return false;
                }
                self.config.fade_duration_ms = next;
                self.save_config();
                true
            }
            Action::BumpStyle(setting, delta) => {
                let (value, min, max) = match setting {
                    panel::StyleSetting::LeftWidth => (&mut self.config.left_width, 48, 800),
                    panel::StyleSetting::RightWidth => (&mut self.config.right_width, 48, 800),
                    panel::StyleSetting::TopHeight => (&mut self.config.top_height, 40, 400),
                    panel::StyleSetting::BottomHeight => (&mut self.config.bottom_height, 40, 400),
                    panel::StyleSetting::Margin => (&mut self.config.margin, 0, 80),
                    panel::StyleSetting::Radius => (&mut self.config.border_radius, 0, 48),
                    panel::StyleSetting::Opacity => (&mut self.config.opacity, 20, 100),
                    panel::StyleSetting::DesktopDim => (&mut self.config.desktop_dim, 0, 100),
                };
                let next = (i64::from(*value) + i64::from(delta)).clamp(min, max) as u32;
                if next == *value {
                    return false;
                }
                *value = next;
                self.save_config();
                self.update_settings_geometry();
                true
            }
            Action::BumpWidth(delta) => {
                let new = config::step_browser_width(self.config.browser_width, delta);
                if new == self.config.browser_width {
                    return false;
                }
                self.config.browser_width = new;
                self.save_config();
                self.browser
                    .set_size(self.config.browser_width, self.config.browser_height);
                log::info!(
                    "browser window default: {}x{}",
                    self.config.browser_width,
                    self.config.browser_height
                );
                true
            }
            Action::BumpHeight(delta) => {
                let new = config::step_browser_height(self.config.browser_height, delta);
                if new == self.config.browser_height {
                    return false;
                }
                self.config.browser_height = new;
                self.save_config();
                self.browser
                    .set_size(self.config.browser_width, self.config.browser_height);
                log::info!(
                    "browser window default: {}x{}",
                    self.config.browser_width,
                    self.config.browser_height
                );
                true
            }
            Action::SettingsContents(_)
            | Action::SelectRectangle(_)
            | Action::SelectSection(_)
            | Action::AddSection
            | Action::RemoveSection
            | Action::SaveSection
            | Action::SectionPage(_)
            | Action::SelectWebsite(_)
            | Action::AddWebsite
            | Action::RemoveWebsite
            | Action::SaveWebsite
            | Action::WebsitePage(_)
            | Action::FocusField(_) => self.edit_contents(action),
        }
    }

    fn edit_contents(&mut self, action: Action) -> bool {
        self.editor_revision = self.editor_revision.wrapping_add(1);
        if let Action::SettingsContents(contents) = action {
            self.ui.contents_tab = contents;
            self.ui.editor.focus = None;
            self.ui.recording_shortcut = false;
            return true;
        }
        if self.ui.editor.apply(action, &mut self.config) {
            self.ui.collapsed.clear();
            self.ui.rectangle_scroll = [0; 4];
            self.ui.selected = None;
            self.refresh_websites();
            self.ui.editor.message = match self.config.save() {
                Ok(()) => "Saved".into(),
                Err(err) => format!("Could not save: {err}"),
            };
        }
        self.ui.hover = None;
        true
    }

    /// Refresh labels and favicon slots after a URL is added, changed or removed.
    fn refresh_websites(&mut self) {
        self.labels = self.config.websites().map(|site| site.label()).collect();
        let old: std::collections::HashMap<_, _> =
            self.icon_urls.drain(..).zip(self.icons.drain(..)).collect();
        self.icon_urls = self
            .config
            .websites()
            .map(|site| site.url.clone())
            .collect();
        self.icons = self
            .icon_urls
            .iter()
            .map(|url| old.get(url).cloned().flatten())
            .collect();
        let targets: std::collections::HashSet<_> = self
            .icon_urls
            .iter()
            .filter(|url| !old.contains_key(*url))
            .cloned()
            .collect();
        if !targets.is_empty() {
            if let Some(tx) = &self.favicon_sender {
                favicon::spawn(targets.into_iter().collect(), tx.clone());
            }
        }
    }

    fn editor_key(&mut self, event: &KeyEvent) {
        use editor::Field;
        let Some(field) = self.ui.editor.focus else {
            return;
        };
        self.editor_revision = self.editor_revision.wrapping_add(1);
        if self.modifiers.logo || self.modifiers.alt {
            return;
        }
        let input = self.ui.editor.input_mut(field);
        if self.modifiers.ctrl {
            if matches!(event.keysym, Keysym::a | Keysym::A) {
                input.selected_all = true;
                input.cursor = input.text.len();
            } else if matches!(event.keysym, Keysym::v | Keysym::V) {
                if let Some(tx) = &self.clipboard_sender {
                    clipboard::paste(self.editor_revision, field, tx.clone());
                }
            }
        } else {
            match event.keysym {
                Keysym::BackSpace => input.delete(true),
                Keysym::Delete => input.delete(false),
                Keysym::Left => input.move_cursor(false),
                Keysym::Right => input.move_cursor(true),
                Keysym::Home => {
                    input.cursor = 0;
                    input.selected_all = false;
                }
                Keysym::End => {
                    input.cursor = input.text.len();
                    input.selected_all = false;
                }
                Keysym::Escape => self.ui.editor.focus = None,
                Keysym::Tab | Keysym::ISO_Left_Tab => {
                    let fields =
                        if self.ui.editor.website.is_some() || self.ui.editor.adding_website {
                            &[Field::SectionName, Field::WebsiteTitle, Field::WebsiteUrl][..]
                        } else {
                            &[Field::SectionName][..]
                        };
                    let index = fields.iter().position(|&f| f == field).unwrap_or(0);
                    let next = if self.modifiers.shift {
                        (index + fields.len() - 1) % fields.len()
                    } else {
                        (index + 1) % fields.len()
                    };
                    self.ui.editor.focus = Some(fields[next]);
                }
                Keysym::Return | Keysym::KP_Enter => {
                    self.edit_contents(if field == Field::SectionName {
                        Action::SaveSection
                    } else {
                        Action::SaveWebsite
                    });
                }
                _ => {
                    if let Some(text) = &event.utf8 {
                        input.insert(text);
                    }
                }
            }
        }
        self.draw();
    }

    /// Persist the config (best effort — failures are logged, not fatal).
    fn save_config(&self) {
        if let Err(err) = self.config.save() {
            log::warn!("could not save config: {err:#}");
        }
    }

    /// Store an arrived favicon and repaint (when the panel is visible).
    fn on_favicon(&mut self, event: favicon::Event) {
        let favicon::Event::Ready { url, icon } = event;
        for (index, target) in self.icon_urls.iter().enumerate() {
            if target == &url {
                self.icons[index] = icon.clone();
            }
        }
        self.draw();
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
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        let mut found = false;
        for panel in [&mut self.panel, &mut self.backdrop, &mut self.settings]
            .into_iter()
            .flatten()
        {
            if panel.layer.wl_surface() == surface {
                panel.frame_pending = false;
                found = true;
                break;
            }
        }
        if found {
            self.draw_surfaces(Some(surface));
        }
        // A callback for a surface hidden during its fade is harmless.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if self
            .panel
            .as_ref()
            .is_some_and(|p| p.layer.wl_surface() == surface)
        {
            self.show_backdrop(output);
        }
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
        if self.panel.as_ref().map(|panel| &panel.layer) == Some(layer)
            || self.backdrop.as_ref().map(|panel| &panel.layer) == Some(layer)
        {
            self.hide();
            log::debug!("panel closed by the compositor");
        } else if self.settings.as_ref().map(|panel| &panel.layer) == Some(layer) {
            self.settings = None;
            self.ui.recording_shortcut = false;
            self.ui.settings_open = false;
            self.ui.hover = None;
            self.draw();
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
        let is_frame = self.panel.as_ref().map(|p| &p.layer) == Some(layer);
        let panel = if self.settings.as_ref().map(|p| &p.layer) == Some(layer) {
            self.settings.as_mut()
        } else if self.backdrop.as_ref().map(|p| &p.layer) == Some(layer) {
            self.backdrop.as_mut()
        } else {
            self.panel.as_mut().filter(|p| &p.layer == layer)
        };
        let Some(panel) = panel else {
            return;
        };
        panel.width = NonZeroU32::new(configure.new_size.0).map_or(panel.width, NonZeroU32::get);
        panel.height = NonZeroU32::new(configure.new_size.1).map_or(panel.height, NonZeroU32::get);
        panel.configured = true;
        if is_frame {
            self.update_settings_geometry();
        }
        self.draw();
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wayland_client::protocol::wl_seat::WlSeat,
    ) {
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wayland_client::protocol::wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer.is_none() => {
                match self.seat_state.get_pointer(qh, &seat) {
                    Ok(pointer) => {
                        log::debug!("pointer ready");
                        self.pointer = Some(pointer);
                    }
                    Err(err) => log::warn!("could not create a pointer: {err}"),
                }
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                match self.seat_state.get_keyboard(qh, &seat, None) {
                    Ok(keyboard) => self.keyboard = Some(keyboard),
                    Err(err) => log::warn!("could not create keyboard: {err}"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wayland_client::protocol::wl_seat::WlSeat,
        capability: Capability,
    ) {
        if matches!(capability, Capability::Pointer) {
            self.pointer = None;
        }
        if matches!(capability, Capability::Keyboard) {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
            self.modifiers = Modifiers::default();
            self.ui.recording_shortcut = false;
            self.draw();
        }
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wayland_client::protocol::wl_seat::WlSeat,
    ) {
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.modifiers = Modifiers::default();
        if self.ui.recording_shortcut {
            self.ui.recording_shortcut = false;
            self.ui.shortcut_message = "Shortcut change cancelled".into();
            self.draw();
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.ui.settings_open && self.ui.contents_tab {
            self.editor_key(&event);
            return;
        }
        if !self.ui.recording_shortcut {
            self.launcher_key(event.keysym, false);
            return;
        }
        if event.keysym == Keysym::Escape
            && !self.modifiers.ctrl
            && !self.modifiers.alt
            && !self.modifiers.logo
        {
            self.ui.recording_shortcut = false;
            self.ui.shortcut_message = "Shortcut change cancelled".into();
        } else {
            match hotkey::captured_shortcut(event.keysym, self.modifiers) {
                Ok(None) => return,
                Ok(Some(shortcut)) => {
                    self.ui.recording_shortcut = false;
                    if self
                        .shortcut_requests
                        .as_ref()
                        .is_some_and(|tx| tx.send(shortcut).is_ok())
                    {
                        self.ui.shortcut_pending = true;
                        self.ui.shortcut_message.clear();
                    } else {
                        self.ui.shortcut_message = "Shortcut service unavailable".into();
                    }
                }
                Err(err) => self.ui.shortcut_message = err.to_string(),
            }
        }
        self.draw();
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.ui.settings_open && self.ui.contents_tab {
            self.editor_key(&event);
        } else {
            self.launcher_key(event.keysym, true);
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let mut hover = self.ui.hover;
        let mut pressed = None;
        for event in events {
            let (x, y) = event.position;
            let is_settings = self
                .settings
                .as_ref()
                .is_some_and(|p| p.layer.wl_surface() == &event.surface);
            let surface = if is_settings {
                self.settings.as_ref()
            } else {
                self.panel
                    .as_ref()
                    .filter(|p| p.layer.wl_surface() == &event.surface)
            };
            let Some(surface) = surface else {
                continue;
            };
            let hits = if is_settings {
                panel::settings_hits(surface.width, surface.height, &self.ui, &self.config)
            } else {
                panel::frame_layout(
                    surface.width,
                    surface.height,
                    self.dimensions(),
                    self.config.margin,
                    &self.ui,
                    &self.config.sections,
                )
            };
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    hover = panel::hit_test(&hits, x, y);
                }
                PointerEventKind::Leave { .. } => hover = None,
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    pressed = panel::hit_test(&hits, x, y);
                }
                _ => {}
            }
        }

        let mut redraw = false;
        if hover != self.ui.hover {
            self.ui.hover = hover;
            redraw = true;
        }
        if let Some(action) = pressed {
            log::debug!("press -> {action:?}");
            redraw |= self.activate(action);
        }
        if redraw {
            self.draw();
        }
    }
}

// KWin supports exporting layer-shell surfaces as toplevel equivalents. The
// exported object's lifetime matches the backdrop and survives browser resizes.
wayland_client::delegate_noop!(App: ignore ZxdgExporterV2);

impl Dispatch<ZxdgExportedV2, ()> for App {
    fn event(
        state: &mut Self,
        exported: &ZxdgExportedV2,
        event: zxdg_exported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.backdrop_export.as_ref() != Some(exported) {
            return;
        }
        if let zxdg_exported_v2::Event::Handle { handle } = event {
            state.browser.set_parent(&handle);
            state.draw();
        }
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);

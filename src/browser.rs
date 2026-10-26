//! The browser: a separate, pre-warmed **process** hosting a wry/WebKitGTK
//! window.
//!
//! The panel spawns `sidepanel --browser <width> <height>` at startup. The
//! helper creates its window and WebKit view immediately but keeps it hidden
//! on `about:blank`: no page is loaded and nothing is shown until a bookmark
//! is clicked, but the view (and, best effort, its web process) is already
//! warm.
//!
//! Commands travel over the helper's stdin, one per line:
//!
//! ```text
//! <title>\t<url>      show/raise the window and navigate
//! hide                 dismiss the window (keeps it warm, off-screen)
//! resume               reveal the same page without navigation
//! restore <json>       prepare a saved session (loaded lazily on resume)
//! reset <generation>   discard the old view and all session state
//! parent <handle>      stack the browser above the exported dimming surface
//! parent               forget the previous parent (it is being unmapped)
//! size <w> <h>         resize without replacing the webview
//! ```
//!
//! On a navigation command the helper presents the window first (so it shows
//! immediately), then loads. Closing the window only hides it, keeping the
//! view warm for the next open. The helper exits when stdin closes (the panel
//! went away).
//!
//! Process isolation is deliberate: WebKitGTK aborts its host process on some
//! failures (missing helper binaries, for instance), and the low-latency
//! panel daemon must not go down with it. If the helper dies, the next click
//! restarts it (at the last configured size).

use crate::{geometry, persistence};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::io::{BufRead, BufWriter, Write};
use std::os::unix::fs::DirBuilderExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::config;
use gtk::gdk::prelude::MonitorExt;
use gtk::glib;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use webkit2gtk::{
    BackForwardListExt, NavigationPolicyDecision, NavigationPolicyDecisionExt, NavigationType,
    PolicyDecisionExt, PolicyDecisionType, URIRequestExt, WebViewExt,
};
use wry::{WebViewBuilder, WebViewBuilderExtUnix, WebViewExtUnix};

/// Durable browser state. During ordinary toggles the actual WebView stays alive.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Snapshot {
    pub open: bool,
    pub url: Option<String>,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    pub fullscreen: bool,
    pub geometry: Option<geometry::Geometry>,
    pub history: Vec<u8>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            open: false,
            url: None,
            title: "Browser".into(),
            width: 1280,
            height: 800,
            maximized: false,
            fullscreen: false,
            geometry: None,
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    pub generation: u64,
    pub snapshot: Snapshot,
}

/// Only route web links with a different hostname to the desktop browser.
/// Compare parsed hosts, so lookalike prefixes cannot count as the same site.
fn is_external_web_link(current: &str, target: &str) -> bool {
    let (Ok(current), Ok(target)) = (url::Url::parse(current), url::Url::parse(target)) else {
        return false;
    };
    matches!(current.scheme(), "http" | "https")
        && matches!(target.scheme(), "http" | "https")
        && current.host().is_some()
        && target.host().is_some()
        && current.host() != target.host()
}

fn open_external(url: String) {
    // xdg-open may wait for the desktop browser. Keep GTK responsive and reap
    // the child when it finishes. Pass the URL as an argument, never a shell.
    std::thread::spawn(move || {
        match Command::new("xdg-open")
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => log::warn!("browser: xdg-open failed for {url}: {status}"),
            Err(err) => log::warn!("browser: could not open {url} externally: {err}"),
        }
    });
}

fn install_link_routing(webview: &wry::WebView) {
    webview
        .webview()
        .connect_decide_policy(|view, decision, kind| {
            if !matches!(
                kind,
                PolicyDecisionType::NavigationAction | PolicyDecisionType::NewWindowAction
            ) {
                return false;
            }
            let Some(policy) = decision.dynamic_cast_ref::<NavigationPolicyDecision>() else {
                return false;
            };
            let Some(mut action) = policy.navigation_action() else {
                return false;
            };
            // Leave automatic redirects, forms, reloads, and bookmark loads alone.
            // A user gesture also covers JavaScript-driven links/window.open().
            if action.is_redirect()
                || !(action.navigation_type() == NavigationType::LinkClicked
                    || (action.navigation_type() == NavigationType::Other
                        && action.is_user_gesture()))
            {
                return false;
            }
            let Some(target) = action.request().and_then(|request| request.uri()) else {
                return false;
            };
            let Some(current) = view.uri() else {
                return false;
            };
            if is_external_web_link(&current, &target) {
                decision.ignore();
                open_external(target.to_string());
                return true;
            }
            if kind == PolicyDecisionType::NewWindowAction
                && url::Url::parse(&target)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
            {
                // Same-site target="_blank" links stay in this single window.
                decision.ignore();
                view.load_uri(&target);
                return true;
            }
            false
        });
}

/// Argument selecting the helper mode in `main`.
pub const ARG: &str = "--browser";

/// Directories where distros place WebKitGTK's helper processes.
const WEBKIT_HELPER_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib/arm-linux-gnueabihf",
    "/usr/lib/riscv64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/usr/libexec",
];

/// WebKitGTK execs its helpers from an absolute path baked in at build time
/// and aborts the process when they are missing — so probe before starting.
fn webkit_runtime_present() -> bool {
    WEBKIT_HELPER_DIRS.iter().any(|dir| {
        let base = std::path::Path::new(dir).join("webkit2gtk-4.1");
        base.join("WebKitNetworkProcess").is_file() && base.join("WebKitWebProcess").is_file()
    })
}

/// Handle used by the panel to talk to the browser helper process.
pub struct Browser {
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    disabled: bool,
    /// Last window size (used for the spawn and respawned helpers).
    width: u32,
    height: u32,
    /// Whether we believe a window is on screen (drives `dismiss`).
    window_open: bool,
    parent_handle: Option<String>,
    waiting_for_parent: bool,
    pending_open: Option<(String, String)>,
    pending_resume: bool,
    snapshot: Snapshot,
    generation: u64,
    sender: Option<calloop::channel::Sender<Event>>,
}

impl Browser {
    /// Start the helper now, hidden, so the first click is instant.
    pub fn start(
        width: u32,
        height: u32,
        snapshot: Snapshot,
        sender: calloop::channel::Sender<Event>,
    ) -> Self {
        let mut browser = Self {
            child: None,
            stdin: None,
            disabled: false,
            width: config::clamp_browser_width(width),
            height: config::clamp_browser_height(height),
            window_open: false,
            parent_handle: None,
            waiting_for_parent: false,
            pending_open: None,
            pending_resume: false,
            snapshot,
            generation: 0,
            sender: Some(sender),
        };
        if !webkit_runtime_present() {
            log::warn!(
                "browser disabled: WebKitGTK runtime not found — install \
                 libwebkit2gtk-4.1-dev (Debian/Ubuntu) and restart the panel"
            );
            browser.disabled = true;
            return browser;
        }
        browser.ensure_child();
        browser
    }

    /// Show the browser window and navigate to `url`, restarting a crashed
    /// helper once on the way.
    pub fn open(&mut self, url: &str, title: &str) {
        if self.disabled {
            log::warn!("browser is disabled; not opening {url}");
            return;
        }
        if self.waiting_for_parent {
            self.pending_open = Some((url.to_string(), title.to_string()));
            return;
        }
        self.snapshot.open = true;
        self.snapshot.url = Some(url.to_string());
        self.snapshot.title = title.to_string();
        let title = title.replace(['\n', '\r', '\t'], " ");
        let command = format!("{title}\t{url}\n");
        if self.write(&command) {
            self.window_open = true;
            return;
        }
        self.forget_helper();
        self.ensure_child();
        if self.write(&command) {
            self.window_open = true;
        } else {
            log::warn!("browser helper is not responding; gave up on {url}");
        }
    }

    /// Defer navigation until KWin has supplied this frame's backdrop handle.
    pub fn wait_for_parent(&mut self) {
        self.clear_parent();
        self.waiting_for_parent = true;
    }

    pub fn set_parent(&mut self, handle: &str) {
        self.waiting_for_parent = false;
        self.parent_handle = Some(handle.to_string());
        if !self.write(&format!("parent {handle}\n")) {
            self.forget_helper();
        }
        if let Some((url, title)) = self.pending_open.take() {
            self.pending_resume = false;
            self.open(&url, &title);
        } else if self.pending_resume {
            self.pending_resume = false;
            self.resume();
        }
    }

    pub fn clear_parent(&mut self) {
        self.waiting_for_parent = false;
        self.parent_handle = None;
        self.pending_open = None;
        self.pending_resume = false;
        let _ = self.write("parent\n");
    }

    pub fn is_open(&self) -> bool {
        self.window_open || self.pending_open.is_some() || self.pending_resume
    }

    /// Dismiss an on-screen browser window (Meta+F also does this). Keeps the
    /// helper — and its warm view — alive.
    pub fn dismiss(&mut self) {
        self.pending_open = None;
        self.pending_resume = false;
        if self.disabled {
            return;
        }
        if self.write("hide\n") {
            self.window_open = false;
        } else {
            // The helper is gone; drop it so the next open respawns cold.
            self.forget_helper();
        }
    }

    /// Reveal the existing view, preserving history, scroll, forms and scripts.
    pub fn resume(&mut self) {
        if self.disabled || !self.snapshot.open {
            return;
        }
        if self.waiting_for_parent {
            self.pending_resume = true;
            return;
        }
        self.ensure_child();
        if !self.write("resume\n") {
            self.forget_helper();
            self.ensure_child();
            self.window_open = self.write("resume\n");
        } else {
            self.window_open = true;
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    pub fn on_event(&mut self, event: &Event, visible: bool) -> bool {
        if event.generation != self.generation {
            return false;
        }
        self.snapshot = event.snapshot.clone();
        self.window_open = visible && self.snapshot.open;
        true
    }

    /// Invalidate outstanding helper events so an expired cache cannot reappear.
    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.snapshot = Snapshot {
            width: self.width,
            height: self.height,
            ..Snapshot::default()
        };
        self.pending_open = None;
        self.pending_resume = false;
        self.window_open = false;
        // Stop the old WebKit processes before removing their profile. A cold
        // helper is appropriate only when the retention boundary is reached.
        self.forget_helper();
        persistence::clear_browser_data();
        if !self.disabled {
            self.ensure_child();
        }
    }

    /// Change the default window size. Applied immediately if the helper is
    /// alive (without replacing its webview); otherwise remembered for the
    /// next spawn.
    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = config::clamp_browser_width(width);
        self.height = config::clamp_browser_height(height);
        self.snapshot.width = self.width;
        self.snapshot.height = self.height;
        if self.disabled {
            return;
        }
        if !self.write(&format!("size {} {}\n", self.width, self.height)) {
            self.forget_helper();
        }
    }

    /// Returns whether the command was accepted by the helper.
    fn write(&mut self, command: &str) -> bool {
        match self.stdin.as_mut() {
            Some(stdin) => stdin
                .write_all(command.as_bytes())
                .and_then(|()| stdin.flush())
                .is_ok(),
            None => false,
        }
    }

    /// Drop a dead helper (reaping it) without spawning a new one.
    fn forget_helper(&mut self) {
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            // EOF lets GTK release the temporary KWin script normally.
            for _ in 0..20 {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
            geometry::cleanup(pid);
        }
        self.generation = self.generation.wrapping_add(1);
        self.window_open = false;
    }

    fn ensure_child(&mut self) {
        if self.stdin.is_some() {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                log::error!("could not locate own binary for the browser helper: {err}");
                return;
            }
        };
        match Command::new(exe)
            .arg(ARG)
            .arg(self.width.to_string())
            .arg(self.height.to_string())
            .env("GDK_BACKEND", "wayland")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Helper logs (env_logger) go to the same stderr as ours.
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(mut child) => {
                let stdin = child.stdin.take().map(BufWriter::new);
                if let (Some(stdout), Some(sender)) = (child.stdout.take(), self.sender.clone()) {
                    std::thread::spawn(move || {
                        for line in std::io::BufReader::new(stdout).lines() {
                            let Ok(line) = line else {
                                break;
                            };
                            if let Ok(event) = serde_json::from_str::<Event>(&line) {
                                if sender.send(event).is_err() {
                                    break;
                                }
                            }
                        }
                    });
                }
                self.child = Some(child);
                self.stdin = stdin;
                let event = Event {
                    generation: self.generation,
                    snapshot: self.snapshot.clone(),
                };
                if let Ok(json) = serde_json::to_string(&event) {
                    let _ = self.write(&format!("restore {json}\n"));
                }
                // A restarted helper must import the parent before navigation.
                if let Some(handle) = self.parent_handle.clone() {
                    let _ = self.write(&format!("parent {handle}\n"));
                }
                log::info!(
                    "browser helper started (hidden, {}x{})",
                    self.width,
                    self.height
                );
            }
            Err(err) => log::error!("could not start the browser helper: {err}"),
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.forget_helper();
    }
}

/// Entry point of the helper process (`sidepanel --browser [w h]`).
///
/// Runs the GTK event loop on the main thread; a reader thread turns stdin
/// lines into wake-ups so GTK keeps pumping while waiting for commands.
/// Exits when stdin closes (panel gone).
pub fn run(width: Option<u32>, height: Option<u32>) -> anyhow::Result<()> {
    if !webkit_runtime_present() {
        anyhow::bail!("WebKitGTK runtime not found");
    }
    gtk::init().map_err(|err| anyhow::anyhow!("gtk::init failed: {err}"))?;
    let placement = geometry::Placement::default();
    let _bridge = match geometry::Bridge::start(placement.clone()) {
        Ok(bridge) => Some(bridge),
        Err(err) => {
            log::warn!("browser placement restoration unavailable: {err:#}");
            None
        }
    };
    let mut defaults = (
        config::clamp_browser_width(width.unwrap_or(1280)),
        config::clamp_browser_height(height.unwrap_or(800)),
    );
    let mut window = Window::create(defaults.0, defaults.1, placement.clone())?;
    prewarm();
    log::info!("browser ready (hidden, about:blank)");

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut generation = 0;
    let mut initialized = false;
    let mut last_sent = None;
    let mut next_snapshot = Instant::now();
    let mut parent_handle: Option<String> = None;
    loop {
        drain_gtk();
        let mut changed = false;
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(line) => match parse(&line) {
                Some(Cmd::Parent { handle }) => {
                    parent_handle = handle.map(str::to_string);
                    if window.inner.is_visible() {
                        if let Some(handle) = &parent_handle {
                            set_foreign_parent(&window.inner, handle)?;
                        }
                    }
                }
                Some(Cmd::Restore(event)) => {
                    generation = event.generation;
                    window.restore(event.snapshot);
                    initialized = true;
                    changed = true;
                }
                Some(Cmd::Resume) => {
                    if window.wanted_open.get() {
                        window.materialize();
                        window.present(parent_handle.as_deref())?;
                    }
                    changed = true;
                }
                Some(Cmd::Navigate { title, url }) => {
                    window.materialize();
                    window.wanted_open.set(true);
                    window.inner.set_title(title);
                    window.present(parent_handle.as_deref())?;
                    if let Err(err) = window.webview.load_url(url) {
                        log::warn!("browser load failed: {err}");
                    }
                    initialized = true;
                    changed = true;
                }
                Some(Cmd::Hide) => {
                    // Keep logical visibility: only a user close cancels reopening.
                    window.inner.hide();
                    changed = true;
                }
                Some(Cmd::Reset(next_generation)) => {
                    generation = next_generation;
                    window.teardown();
                    persistence::clear_browser_data();
                    placement.set(None);
                    window = Window::create(defaults.0, defaults.1, placement.clone())?;
                    initialized = true;
                    changed = true;
                    last_sent = None;
                }
                Some(Cmd::Size { width, height }) => {
                    let width = config::clamp_browser_width(width);
                    let height = config::clamp_browser_height(height);
                    defaults = (width, height);
                    window.width = width;
                    window.height = height;
                    // GTK negotiates an xdg-toplevel resize. Keep the same view
                    // so editing the default size does not reload the page.
                    window.inner.resize(width as i32, height as i32);
                    if let Some(snapshot) = &mut window.pending {
                        snapshot.width = width;
                        snapshot.height = height;
                    }
                    changed = true;
                }
                None => log::warn!("browser: malformed command"),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if initialized && (changed || Instant::now() >= next_snapshot) {
            let snapshot = window.snapshot();
            if last_sent.as_ref() != Some(&snapshot) {
                let event = Event {
                    generation,
                    snapshot: snapshot.clone(),
                };
                let mut stdout = std::io::stdout().lock();
                serde_json::to_writer(&mut stdout, &event)?;
                writeln!(stdout)?;
                stdout.flush()?;
                last_sent = Some(snapshot);
            }
            next_snapshot = Instant::now() + Duration::from_millis(500);
        }
    }
    window.teardown();
    Ok(())
}

struct Window {
    inner: gtk::Window,
    webview: wry::WebView,
    width: u32,
    height: u32,
    wanted_open: Rc<Cell<bool>>,
    placement: geometry::Placement,
    pending: Option<Snapshot>,
    context: Option<wry::WebContext>,
}

impl Window {
    fn create(width: u32, height: u32, placement: geometry::Placement) -> anyhow::Result<Self> {
        let inner = gtk::Window::new(gtk::WindowType::Toplevel);
        inner.set_default_size(width as i32, height as i32);
        inner.set_position(gtk::WindowPosition::Center);
        inner.set_title("Browser");
        let wanted_open = Rc::new(Cell::new(false));
        let closed = wanted_open.clone();
        inner.connect_delete_event(move |win, _| {
            closed.set(false);
            win.hide();
            glib::Propagation::Stop
        });
        let mut context = if let Some(path) = persistence::browser_data_path() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)?;
            Some(wry::WebContext::new(Some(path)))
        } else {
            None
        };
        let builder = match context.as_mut() {
            Some(context) => WebViewBuilder::new_with_web_context(context),
            None => WebViewBuilder::new(),
        };
        let webview = builder
            .with_url("about:blank")
            .build_gtk(&inner)
            .map_err(|err| anyhow::anyhow!("failed to create webview: {err}"))?;
        install_link_routing(&webview);
        inner.realize();
        center_window(&inner, width, height);
        inner.show();
        inner.hide();
        Ok(Self {
            inner,
            webview,
            width,
            height,
            wanted_open,
            placement,
            pending: None,
            context,
        })
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.wanted_open.set(snapshot.open);
        self.width = config::clamp_browser_width(snapshot.width);
        self.height = config::clamp_browser_height(snapshot.height);
        self.inner.resize(self.width as i32, self.height as i32);
        self.inner.set_title(&snapshot.title);
        self.placement.set(snapshot.geometry);
        self.pending = Some(snapshot);
    }

    /// Disk restoration is lazy; ordinary hide/show never navigates or reloads.
    fn materialize(&mut self) {
        let Some(snapshot) = self.pending.take() else {
            return;
        };
        let view = self.webview.webview();
        let mut restored = false;
        if !snapshot.history.is_empty() {
            let bytes = glib::Bytes::from(&snapshot.history);
            // A truncated/invalid cache can produce NULL. Check before wrapping
            // rather than passing it through gtk-rs's non-null constructor.
            unsafe {
                let raw =
                    webkit2gtk::ffi::webkit_web_view_session_state_new(bytes.to_glib_none().0);
                if !raw.is_null() {
                    let state: webkit2gtk::WebViewSessionState =
                        glib::translate::from_glib_full(raw);
                    view.restore_session_state(&state);
                    if let Some(item) = view
                        .back_forward_list()
                        .and_then(|list| list.current_item())
                    {
                        view.go_to_back_forward_list_item(&item);
                        restored = true;
                    }
                }
            }
        }
        if !restored {
            if let Some(url) = snapshot.url {
                view.load_uri(&url);
            }
        }
        if snapshot.maximized {
            self.inner.maximize();
        }
        if snapshot.fullscreen {
            self.inner.fullscreen();
        }
    }

    fn present(&self, parent: Option<&str>) -> anyhow::Result<()> {
        self.inner.present();
        if let Some(handle) = parent {
            set_foreign_parent(&self.inner, handle)?;
        }
        Ok(())
    }

    fn snapshot(&self) -> Snapshot {
        if let Some(snapshot) = &self.pending {
            return snapshot.clone();
        }
        let (width, height) = self.inner.size();
        let view = self.webview.webview();
        let url = view
            .uri()
            .map(|uri| uri.to_string())
            .filter(|uri| uri != "about:blank");
        let history = if url.is_some() {
            view.session_state()
                .and_then(|state| state.serialize())
                .map(|bytes| bytes.as_ref().to_vec())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Snapshot {
            open: self.wanted_open.get(),
            url,
            title: self
                .inner
                .title()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            width: width.max(1) as u32,
            height: height.max(1) as u32,
            maximized: self.inner.is_maximized(),
            fullscreen: self
                .inner
                .window()
                .is_some_and(|win| win.state().contains(gtk::gdk::WindowState::FULLSCREEN)),
            geometry: self.placement.get(),
            history,
        }
    }

    fn teardown(self) {
        self.inner.hide();
        drain_gtk();
        drop(self.webview);
        unsafe {
            self.inner.destroy();
        }
        drop(self.inner);
        drop(self.context);
        drain_gtk();
    }
}

/// Commands understood by the helper, one per stdin line.
#[derive(Debug)]
enum Cmd<'a> {
    Parent { handle: Option<&'a str> },
    Navigate { title: &'a str, url: &'a str },
    Hide,
    Resume,
    Restore(Event),
    Reset(u64),
    Size { width: u32, height: u32 },
}

fn parse(line: &str) -> Option<Cmd<'_>> {
    if let Some(json) = line.strip_prefix("restore ") {
        return serde_json::from_str(json).ok().map(Cmd::Restore);
    }
    if let Some((title, url)) = line.split_once('\t') {
        return Some(Cmd::Navigate { title, url });
    }
    let mut parts = line.split_whitespace();
    match parts.next()? {
        "parent" => {
            let handle = parts.next();
            if parts.next().is_some() {
                return None;
            }
            Some(Cmd::Parent { handle })
        }
        "hide" => Some(Cmd::Hide),
        "resume" => Some(Cmd::Resume),
        "reset" => Some(Cmd::Reset(parts.next()?.parse().ok()?)),
        "size" => Some(Cmd::Size {
            width: parts.next()?.parse().ok()?,
            height: parts.next()?.parse().ok()?,
        }),
        _ => None,
    }
}

/// GTK3 exposes cross-process Wayland parenting in its backend C API, but
/// gtk-rs's backend-neutral bindings don't wrap it. The realized GDK window
/// remains owned by GTK; GTK imports and manages the handle itself.
fn set_foreign_parent(window: &gtk::Window, handle: &str) -> anyhow::Result<()> {
    unsafe extern "C" {
        fn gdk_wayland_window_set_transient_for_exported(
            window: *mut gtk::gdk::ffi::GdkWindow,
            handle: *const std::ffi::c_char,
        ) -> gtk::glib::ffi::gboolean;
    }
    let gdk_window = window
        .window()
        .ok_or_else(|| anyhow::anyhow!("browser window is not realized"))?;
    let handle = std::ffi::CString::new(handle)?;
    // The helper is launched with GDK_BACKEND=wayland; both pointers are valid
    // for the call and GTK makes its own imported-surface object.
    let accepted = unsafe {
        gdk_wayland_window_set_transient_for_exported(gdk_window.to_glib_none().0, handle.as_ptr())
    };
    anyhow::ensure!(
        accepted != 0,
        "could not place the browser above the dimming surface"
    );
    Ok(())
}

fn drain_gtk() {
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
}

/// Pump GTK for a short while so the freshly created, unmapped window makes
/// progress (about:blank load, web-process spawn) without ever being shown.
fn prewarm() {
    for _ in 0..50 {
        drain_gtk();
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Ask GTK to center the window, then place it on the monitor workarea.
/// Wayland compositors may ignore `move_`; destroying the previous window
/// first still lets them treat this as a new, centered placement.
fn center_window(window: &gtk::Window, width: u32, height: u32) {
    window.set_position(gtk::WindowPosition::Center);
    let Some(gdk_window) = window.window() else {
        return;
    };
    let Some(monitor) = window
        .display()
        .monitor_at_window(&gdk_window)
        .or_else(|| window.display().primary_monitor())
    else {
        return;
    };
    let area = monitor.workarea();
    let x = area.x() + (area.width() - width as i32) / 2;
    let y = area.y() + (area.height() - height as i32) / 2;
    window.move_(x, y);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_links_are_routed_by_parsed_hostname() {
        let current = "https://example.com/inbox";
        for target in [
            "https://example.com/message/1",
            "https://EXAMPLE.com/inbox#unread",
            "http://example.com:8080/",
            "https://example.com/?next=https://other.test/",
        ] {
            assert!(!is_external_web_link(current, target), "{target}");
        }
        for target in [
            "https://other.test/",
            "https://sub.example.com/",
            "https://example.com.evil.test/",
            "https://example.com@evil.test/",
        ] {
            assert!(is_external_web_link(current, target), "{target}");
        }
    }

    #[test]
    fn initial_loads_and_non_web_urls_are_not_sent_to_xdg_open() {
        for (current, target) in [
            ("about:blank", "https://example.com/"),
            ("invalid", "https://example.com/"),
            ("https://example.com/", "invalid"),
            ("https://example.com/", "mailto:hello@example.com"),
            ("https://example.com/", "javascript:void(0)"),
            ("https://example.com/", "file:///tmp/example.html"),
            ("https://example.com/", "about:blank"),
        ] {
            assert!(
                !is_external_web_link(current, target),
                "{current} -> {target}"
            );
        }
        assert!(!is_external_web_link(
            "http://127.0.0.1/",
            "http://127.0.0.1:8000/"
        ));
        assert!(is_external_web_link(
            "http://127.0.0.1/",
            "http://127.0.0.2/"
        ));
    }

    #[test]
    fn parent_waiting_queues_navigation_and_dismiss_cancels_it() {
        let mut browser = Browser {
            child: None,
            stdin: None,
            disabled: false,
            width: 1280,
            height: 800,
            window_open: false,
            parent_handle: None,
            waiting_for_parent: false,
            pending_open: None,
            pending_resume: false,
            snapshot: Snapshot::default(),
            generation: 0,
            sender: None,
        };
        browser.wait_for_parent();
        browser.open("https://example.com/", "Example");
        assert!(browser.is_open());
        assert_eq!(
            browser.pending_open.as_ref().unwrap().0,
            "https://example.com/"
        );
        browser.open("https://example.com/next", "Next");
        assert_eq!(browser.pending_open.as_ref().unwrap().1, "Next");
        browser.dismiss();
        assert!(!browser.is_open());
        browser.clear_parent();
        assert!(!browser.waiting_for_parent);
        browser.set_parent("backdrop-handle");
        assert_eq!(browser.parent_handle.as_deref(), Some("backdrop-handle"));
        browser.clear_parent();
        assert!(browser.parent_handle.is_none());

        browser.snapshot.open = true;
        browser.snapshot.url = Some("https://example.com/current".into());
        browser.wait_for_parent();
        browser.resume();
        assert!(browser.pending_resume);
        assert!(browser.is_open());
        browser.dismiss();
        assert!(!browser.pending_resume);
        assert!(
            browser.snapshot.open,
            "shortcut hide preserves logical visibility"
        );
        let stale = Event {
            generation: browser.generation,
            snapshot: browser.snapshot.clone(),
        };
        browser.generation = browser.generation.wrapping_add(1);
        browser.snapshot = Snapshot::default();
        assert!(
            !browser.on_event(&stale, true),
            "late events must not resurrect expired sessions"
        );
        assert!(!browser.snapshot.open);
        let closed = Event {
            generation: browser.generation,
            snapshot: Snapshot {
                open: false,
                ..stale.snapshot
            },
        };
        assert!(browser.on_event(&closed, true));
        browser.wait_for_parent();
        browser.resume();
        assert!(!browser.pending_resume, "a user-closed window stays closed");
    }

    #[test]
    fn parses_navigation_commands() {
        match parse("My Site\thttp://localhost:6005/") {
            Some(Cmd::Navigate { title, url }) => {
                assert_eq!(title, "My Site");
                assert_eq!(url, "http://localhost:6005/");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_control_commands() {
        assert!(matches!(parse("hide"), Some(Cmd::Hide)));
        assert!(matches!(parse("resume"), Some(Cmd::Resume)));
        assert!(matches!(parse("reset 42"), Some(Cmd::Reset(42))));
        let event = Event {
            generation: 42,
            snapshot: Snapshot::default(),
        };
        assert!(matches!(
            parse(&format!(
                "restore {}",
                serde_json::to_string(&event).unwrap()
            )),
            Some(Cmd::Restore(Event { generation: 42, .. }))
        ));
        assert!(matches!(
            parse("parent backdrop-handle"),
            Some(Cmd::Parent {
                handle: Some("backdrop-handle")
            })
        ));
        assert!(matches!(
            parse("parent"),
            Some(Cmd::Parent { handle: None })
        ));
        assert!(parse("parent handle extra").is_none());
        match parse("size 1600 900") {
            Some(Cmd::Size { width, height }) => {
                assert_eq!((width, height), (1600, 900));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(parse("size banana").is_none());
        assert!(parse("nonsense").is_none());
    }
}

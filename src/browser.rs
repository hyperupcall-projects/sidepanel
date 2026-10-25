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
//! parent <handle>      stack the browser above the exported dimming surface
//! parent               forget the previous parent (it is being unmapped)
//! size <w> <h>         destroy the old window, then create one at the new size
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

use std::io::{BufRead, BufWriter, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use crate::config;
use gtk::gdk::prelude::MonitorExt;
use gtk::glib;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use webkit2gtk::{
    NavigationPolicyDecision, NavigationPolicyDecisionExt, NavigationType, PolicyDecisionExt,
    PolicyDecisionType, URIRequestExt, WebViewExt,
};
use wry::{WebViewBuilder, WebViewBuilderExtUnix, WebViewExtUnix};

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
}

impl Browser {
    /// Start the helper now, hidden, so the first click is instant.
    pub fn start(width: u32, height: u32) -> Self {
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
            self.open(&url, &title);
        }
    }

    pub fn clear_parent(&mut self) {
        self.waiting_for_parent = false;
        self.parent_handle = None;
        self.pending_open = None;
        let _ = self.write("parent\n");
    }

    pub fn is_open(&self) -> bool {
        self.window_open || self.pending_open.is_some()
    }

    /// Dismiss an on-screen browser window (Meta+F also does this). Keeps the
    /// helper — and its warm view — alive.
    pub fn dismiss(&mut self) {
        self.pending_open = None;
        if !self.window_open || self.disabled {
            return;
        }
        if self.write("hide\n") {
            self.window_open = false;
        } else {
            // The helper is gone; drop it so the next open respawns cold.
            self.forget_helper();
        }
    }

    /// Change the default window size. Applied immediately if the helper is
    /// alive (it rebuilds its hidden window); otherwise remembered for the
    /// next spawn.
    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = config::clamp_browser_width(width);
        self.height = config::clamp_browser_height(height);
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
            let _ = child.kill();
            let _ = child.wait();
        }
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
            .stdout(Stdio::null())
            // Helper logs (env_logger) go to the same stderr as ours.
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(mut child) => {
                let stdin = child.stdin.take().map(BufWriter::new);
                self.child = Some(child);
                self.stdin = stdin;
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
        anyhow::bail!(
            "WebKitGTK runtime not found — install libwebkit2gtk-4.1-dev \
             (Debian/Ubuntu) or the equivalent"
        );
    }
    if let Err(err) = gtk::init() {
        anyhow::bail!("gtk::init failed: {err}");
    }

    let mut window = Window::create(
        config::clamp_browser_width(width.unwrap_or(1280)),
        config::clamp_browser_height(height.unwrap_or(800)),
    )?;
    // Warm-up: let the queued `about:blank` load and web-process spawn make
    // progress while still unmapped.
    prewarm();
    log::info!(
        "browser ready (hidden, about:blank, {}x{})",
        window.width,
        window.height
    );

    // stdin -> channel, so the main loop can multiplex with GTK.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    log::warn!("browser stdin error: {err}");
                    break;
                }
            }
        }
        // Dropping `tx` tells the main loop the panel is gone.
    });

    /// The helper's window + view, plus the last navigation target.
    struct Window {
        inner: gtk::Window,
        webview: wry::WebView,
        width: u32,
        height: u32,
        last_url: Option<String>,
    }

    impl Window {
        fn create(width: u32, height: u32) -> anyhow::Result<Self> {
            let inner = gtk::Window::new(gtk::WindowType::Toplevel);
            inner.set_default_size(width as i32, height as i32);
            inner.set_position(gtk::WindowPosition::Center);
            inner.set_title("Browser");
            // Closing hides rather than destroys: the view stays warm.
            inner.connect_delete_event(|win, _| {
                win.hide();
                glib::Propagation::Stop
            });
            let webview = WebViewBuilder::new()
                .with_url("about:blank")
                .build_gtk(&inner)
                .map_err(|err| anyhow::anyhow!("failed to create the webview: {err}"))?;
            install_link_routing(&webview);
            // Realise off-screen: `realize` builds the widget tree, and
            // show/hide before any GTK iteration queues no Wayland commit —
            // no flash, no mapped surface.
            inner.realize();
            center_window(&inner, width, height);
            inner.show();
            inner.hide();
            Ok(Self {
                inner,
                webview,
                width,
                height,
                last_url: None,
            })
        }

        /// Unmap and destroy this toplevel. Dropping the Rust handle only
        /// unrefs it; GTK keeps toplevels in its window list until
        /// `gtk_widget_destroy`, which is why a resize used to leave the old
        /// window on screen.
        fn teardown(self) {
            self.inner.hide();
            drain_gtk();
            drop(self.webview);
            unsafe {
                self.inner.destroy();
            }
            drop(self.inner);
            drain_gtk();
        }
    }

    let mut parent_handle: Option<String> = None;
    loop {
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
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
                Some(Cmd::Navigate { title, url }) => {
                    log::info!("browser: opening {url}");
                    window.inner.set_title(title);
                    // Present first so the window shows immediately, then load.
                    window.inner.present();
                    // GTK may reset the GDK parent while mapping a toplevel.
                    // Import after present, before pumping the next GTK frame.
                    if let Some(handle) = &parent_handle {
                        set_foreign_parent(&window.inner, handle)?;
                    }
                    if let Err(err) = window.webview.load_url(url) {
                        log::warn!("browser: loading {url} failed: {err}");
                    }
                    window.last_url = Some(url.to_string());
                }
                Some(Cmd::Hide) => {
                    window.inner.hide();
                    log::debug!("browser: window dismissed (kept warm)");
                }
                Some(Cmd::Size { width, height }) => {
                    let width = config::clamp_browser_width(width);
                    let height = config::clamp_browser_height(height);
                    if width == window.width && height == window.height {
                        continue;
                    }
                    // Wayland clients cannot resize their own toplevels, so
                    // destroy the old window first and create a replacement
                    // at the new size, centered on the current monitor.
                    let was_visible = window.inner.is_visible();
                    let previous = window.last_url.clone();
                    log::info!("browser: resizing to {width}x{height}");
                    window.teardown();
                    match Window::create(width, height) {
                        Ok(mut fresh) => {
                            prewarm();
                            fresh.last_url = previous.clone();
                            if was_visible {
                                fresh.inner.present();
                                if let Some(handle) = &parent_handle {
                                    set_foreign_parent(&fresh.inner, handle)?;
                                }
                                center_window(&fresh.inner, width, height);
                                if let Some(url) = previous {
                                    if let Err(err) = fresh.webview.load_url(&url) {
                                        log::warn!("browser: reload failed: {err}");
                                    }
                                }
                            }
                            window = fresh;
                        }
                        Err(err) => {
                            log::error!("browser: resize failed: {err}");
                            break;
                        }
                    }
                }
                None => log::warn!("browser: malformed command {line:?}"),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    log::info!("browser helper exiting");
    Ok(())
}

/// Commands understood by the helper, one per stdin line.
#[derive(Debug)]
enum Cmd<'a> {
    Parent { handle: Option<&'a str> },
    Navigate { title: &'a str, url: &'a str },
    Hide,
    Size { width: u32, height: u32 },
}

fn parse(line: &str) -> Option<Cmd<'_>> {
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

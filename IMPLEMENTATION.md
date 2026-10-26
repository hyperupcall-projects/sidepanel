# sidepanel — implementation notes

The current launcher renders four edge panels and four corner boxes inside
one transparent, output-sized layer surface. `src/frame.rs` computes the eight
rectangles for the renderer, pointer hits, and Wayland input region, so the
desktop remains accessible through the center and gaps. The bottom-left box
opens a separate settings overlay on the same output. The earlier single
sidebar implementation described below is retained as design history.

Settings now has Appearance and Contents tabs. `Config::sections` stores ordered
`Section { rectangle, title, websites }` records for the four edge rectangles;
the `Rectangle` enum excludes corners. `Config::parse` migrates legacy top-level
`[[websites]]` into the left rectangle's Websites section. Explicitly empty
sections remain empty. Saves use `[[sections]]` / `[[sections.websites]]`.

`src/editor.rs` owns section/website selection, paginated lists, separate name,
title and URL drafts, UTF-8 cursor input, and HTTP/HTTPS validation. Rename and
Save/Add website commit drafts; structural add/remove actions persist immediately.
The renderer shares content and scaled settings geometry with pointer hits.
Vertical rails flow downward; horizontal rails flow rightward. Overflow arrows
step through all entries, and each section collapses independently. Website
indices are flattened across sections for keyboard selection and favicon slots.
Favicon events identify URLs, so a result for a deleted/replaced entry cannot
be assigned to an unrelated row. Existing URL icons survive content edits.

The frame keeps keyboard focus while settings is open. Editor keystrokes are
consumed before launcher activation and shortcut recording. Optional Ctrl+V
uses `src/clipboard.rs` to invoke `wl-paste` off the event loop, with bounded
input and a two-second timeout. Replies carry a field and editor revision so
late results cannot paste into a different draft. No additional Rust dependency
is needed; users without wl-clipboard can still type and edit all fields.

`panel_animation` selects Desktop Default, Fade In, or Instant for the layer
surfaces. Desktop Default retains the original namespaces and delegates to
KWin. Custom modes use the layer-shell namespace `dock`, which KWin maps to
its Dock window type, excluded by its standard Scale, Fade, and Fading Popups
effects. This is KDE-specific; other compositors and third-party effects may
interpret the namespace differently. See KWin's
[namespace mapping](https://github.com/KDE/kwin/blob/Plasma/6.3/src/layershellv1window.cpp).
Layer, exclusive zone, keyboard interactivity, and input regions are unchanged.
`src/animation.rs` implements a linear opacity ramp over `fade_duration_ms`
(default 150, clamped to 0–5000). All premultiplied channels are scaled, leaving
geometry and transparent gaps intact. Each surface snapshots its animation
settings when created, starts on its first buffer, requests one Wayland frame
callback at a time, and stops requesting callbacks after reaching full opacity.
The backdrop clones the frame's timeline; settings starts its own. Hiding
destroys the surfaces, so late callbacks cannot animate a reopened panel.
The browser helper is unaffected. Settings changes apply on the next open.

Dimming uses a separate `Layer::Top` surface, filled with premultiplied black
controlled by `desktop_dim` (0–100 percent, default 20), with an empty input
region and no keyboard focus. Cards live in a transparent `Layer::Overlay`
surface. Keeping the backdrop out of the cards' canvas is essential: otherwise
the overlay dims ordinary browser windows beneath it.

KWin supports exporting layer-shell surfaces as toplevel equivalents through
`zxdg_exporter_v2`. The backdrop is exported and its handle sent to the GTK
browser helper. GTK's `gdk_wayland_window_set_transient_for_exported` imports
it, giving KWin a stacking constraint: backdrop below browser, with the overlay
cards and settings above both. See KWin's [foreign surface handling](https://github.com/KDE/kwin/blob/master/src/wayland/xdgforeign_v2.cpp)
and [transient stacking constraints](https://github.com/KDE/kwin/blob/master/src/window.cpp).
The compositor tracks placement, so browser movement and resizing need no
geometry polling or transparent holes. The GTK import is reapplied to resized
windows and restarted helpers. An early navigation request waits for the
exported handle; hiding cancels any pending request and destroys the export.
All surfaces share the frame's actual output. Without `xdg-foreign`, the
launcher disables dimming and continues to open the browser normally.

After navigation the cards accept keyboard focus on demand, allowing browser
typing. Opening the launcher and recording a shortcut retain exclusive focus.
Opaque card colors remain unchanged, the gaps pass pointer input through, and
hiding the frame removes its backdrop. Settings changes persist immediately.

`left_width`, `right_width`, `top_height`, and `bottom_height` independently
control edge thicknesses. Each corner inherits its adjacent width and height.
For an output width `W` and shared margin `m`, horizontal edges span
`W - left_width - right_width - 4*m`; vertical edges use the corresponding
height formula. Two margins inset the frame and two separate the edge panels
from their corner boxes. Opposing dimensions shrink proportionally on small
outputs, and the settings overlay scales to fit while keeping hit coordinates
aligned. The old config `width` key remains an alias for `left_width`.

How the two hard parts of this app were built: **(1)** getting a global keyboard
shortcut on KDE Plasma Wayland, and **(2)** putting a full-height panel on screen
with as little latency as possible. Each part covers the approach, the mistakes
that cost real time, and the code that finally worked.

The whole thing is Wayland-only. There is deliberately no X11 path.

---

## Part 1 — Global keyboard shortcuts via KGlobalAccel

### 1.1 The problem

On X11 you install a global hotkey with `XGrabKey`: the X server sends you the
key no matter which window has focus. **Wayland has no equivalent.** The
compositor owns the keyboard and only delivers key events to the focused
surface, so a client cannot grab a key globally at all.

KDE's answer is **KGlobalAccel**. It is a broker that owns the "which key
triggers which action" table, has KWin perform the actual grab, and notifies the
owning application over D-Bus when the key is pressed. It is the same mechanism
KWin, Plasma, and every KDE app use, and it works on Wayland.

```
Meta+F ──► KWin (owns the keyboard; hosts the KGlobalAccel daemon)
              │
              └── D-Bus signal `globalShortcutPressed` ──► this app
```

### 1.2 The protocol

- Service `org.kde.kglobalaccel`, object `/kglobalaccel`, interface
  `org.kde.KGlobalAccel`.
- An *action* is identified by a four-element `QStringList`:
  `[componentUnique, actionUnique, componentFriendly, actionFriendly]`.
- The methods we need:

| Method | Signature | Purpose |
| --- | --- | --- |
| `doRegister` | `as` | create the action |
| `setShortcutKeys` | `asa(ai)u -> a(ai)` | set/query the binding; returns the binding actually in effect |
| `getComponent` | `s -> o` | get the per-component object used for signals |

- The signal we listen for lives on the component object:
  `org.kde.kglobalaccel.Component.globalShortcutPressed(ssx)`.

### 1.3 Talking to it from Rust with `zbus`

`zbus` generates a typed client from a trait. Two proxies: one for the main
interface, one for the component signal.

```rust
#[zbus::proxy(
    interface = "org.kde.KGlobalAccel",
    default_service = "org.kde.kglobalaccel",
    default_path = "/kglobalaccel"
)]
trait KGlobalAccel {
    #[zbus(name = "doRegister")]
    fn do_register(&self, action_id: &[&str]) -> zbus::Result<()>;

    #[zbus(name = "setShortcutKeys")]
    fn set_shortcut_keys(
        &self,
        action_id: &[&str],
        keys: &[(Vec<i32>,)],
        flags: u32,
    ) -> zbus::Result<Vec<(Vec<i32>,)>>;

    #[zbus(name = "getComponent")]
    fn get_component(&self, component_unique: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.kde.kglobalaccel.Component",
    default_service = "org.kde.kglobalaccel"
)]
trait Component {
    #[zbus(name = "globalShortcutPressed", signal)]
    fn global_shortcut_pressed(
        &self,
        component_unique: &str,
        action_unique: &str,
        timestamp: i64,
    ) -> zbus::Result<()>;
}
```

### 1.4 Pitfall — camelCase method names

`zbus` PascalCases Rust method names by default, so `do_register` would be sent
as `DoRegister`, which the daemon does not implement. The fix is to spell the
D-Bus name out explicitly with `#[zbus(name = "...")]` on every method — visible
in the snippet above. Without it you get a `UnknownMethod` error at runtime.

### 1.5 Pitfall — the wrong array shape

`QKeySequence` is **not** an integer. On the wire it is a *struct* wrapping an
array of ints, so the D-Bus signature is `a(ai)`, not `aai`.

The natural-looking Rust type `Vec<Vec<i32>>` serialises to `aai` and is
rejected. The fix is a one-element tuple, which gives `zbus` the struct wrapper
it needs:

```rust
// aai  — wrong
let keys: Vec<Vec<i32>> = vec![vec![combined]];

// a(ai) — right
let keys: Vec<(Vec<i32>,)> = vec![(vec![combined],)];
```

### 1.6 Pitfall — the one that crashed KWin

This is the mistake worth writing down.

The first working-looking version sent a `QKeySequence` as an array of **one**
int:

```rust
// WRONG — one int, but Qt reads four
let keys = [(vec![combined],)];
```

Running the app **took down the entire Plasma session.** The crash dump
(`kwin_wayland`, `SIGABRT`) showed:

```
#11 __GI_abort
#12 _dbus_warn_check_failed            libdbus-1
#14 QDBusArgument::operator>>(int&)    libQt6DBus
#15 operator>>(QDBusArgument const&, QKeySequence&)   libKF6GlobalAccel
#17 QDBusMetaType::demarshall          libQt6DBus
#22 QObject::event                     libQt6Core   ← KWin's main thread
```

Disassembling the demarshaller on the machine (`libKF6GlobalAccel.so.6`,
`operator>>(QDBusArgument const&, QKeySequence&)`) made it obvious:

```
beginStructure(); beginArray();
operator>>(int&)  ×4          ← reads four ints, unconditionally
QKeySequence(int,int,int,int)
endArray(); endStructure();
```

Qt reads **exactly four** ints and never checks for the end of the array. Our
one-element array made the second read run off the end, which trips a libdbus
assertion, and libdbus calls `abort()`.

Why it killed the *compositor* and not just our app: on Plasma 6 Wayland,
**KWin hosts the KGlobalAccel daemon in-process** (`libkwin.so.6` imports
`KGlobalAccelD::KGlobalAccelD`, `KGlobalAccelD::init`, `KGlobalAccelD::interface`,
and `KGlobalAccelD::init()` registers `org.kde.kglobalaccel` on the session bus).
So the malformed call was demarshalled in KWin's main thread.

The fix is to always pad to four ints:

```rust
/// Encode a Qt "combined" key code as a `QKeySequence` for the D-Bus wire.
///
/// KGlobalAccel marshals a `QKeySequence` as a struct wrapping an array of
/// exactly four int32s — a chord of up to four keys, zero-padded. Qt's
/// demarshaller reads all four unconditionally, so a shorter array makes the
/// receiver read past the end and abort the whole process on a libdbus
/// assertion. Always pad to four.
fn encode_sequence(combined: i32) -> Vec<i32> {
    vec![combined, 0, 0, 0]
}
```

### 1.7 Pitfall — the flags

`setShortcutKeys` takes a `uint` flags argument. The values that matter
(`KGlobalAccelD::SetShortcutFlag`):

| Flag | Value | Meaning |
| --- | --- | --- |
| `SetPresent` | `0x2` | mark the shortcut as active |
| `NoAutoloading` | `0x4` | do not let the daemon keep a saved binding |
| `IsDefault` | `0x8` | record this as the action's default |

The first version passed `0` and `4` and never set `SetPresent`, so the shortcut
was never actually marked active. The reference client
(`KGlobalAccelPrivate::updateGlobalShortcut`) uses `SetPresent` for the active
binding and `IsDefault` for the default. We match it:

```rust
const SET_PRESENT: u32 = 0x2;
const IS_DEFAULT: u32 = 0x8;
```

Deliberately **not** passing `NoAutoloading` gives us the "respect a
user-customised binding" behaviour for free: with autoloading, the daemon keeps
a binding the user set in System Settings and only installs ours when the action
has none yet. Either way it returns the binding actually in effect.

### 1.8 Encoding the key

KGlobalAccel speaks Qt's `QKeySequence` "combined" encoding: modifier flags in
the high bits, the key code in the low bits, OR-ed together. Meta is
`0x1000_0000`, and letters are their uppercase ASCII value, so **Meta+F** is
`0x1000_0046` = `268435526`.

```rust
fn parse_shortcut(spec: &str) -> Result<i32> {
    const SHIFT: i32 = 0x0200_0000;
    const CTRL: i32 = 0x0400_0000;
    const ALT: i32 = 0x0800_0000;
    const META: i32 = 0x1000_0000;

    let mut modifiers = 0i32;
    let mut key: Option<i32> = None;

    for raw in spec.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "meta" | "super" | "logo" | "win" => modifiers |= META,
            "ctrl" | "control" => modifiers |= CTRL,
            "alt" => modifiers |= ALT,
            "shift" => modifiers |= SHIFT,
            other => {
                key = Some(key_code(other).ok_or_else(|| {
                    anyhow!("unsupported key '{other}' in shortcut '{spec}'")
                })?);
            }
        }
    }

    let key = key.ok_or_else(|| anyhow!("shortcut '{spec}' contains no key"))?;
    Ok(modifiers | key)
}
```

`key_code` maps single letters/digits plus a table of named keys (`space`,
`escape`, `f1`..`f12`, arrows, ...) to their `Qt::Key` values.

### 1.9 The finished registration flow

Register the action, set the active binding, set the default, then resolve the
component object so we can subscribe to the signal.

```rust
fn run(sender: Sender<HotkeyEvent>, config: &Config) -> Result<()> {
    let conn = zbus::blocking::Connection::session()
        .context("failed to connect to the session bus")?;
    let kga = KGlobalAccelProxyBlocking::new(&conn)
        .context("KGlobalAccel is unavailable (is kglobalacceld running?)")?;

    let action_id = [COMPONENT, ACTION, COMPONENT_FRIENDLY, ACTION_FRIENDLY];
    kga.do_register(&action_id)
        .context("failed to register the component with KGlobalAccel")?;

    let combined = parse_shortcut(&config.shortcut)?;
    let keys = [(encode_sequence(combined),)];

    // SetPresent marks the shortcut active. Not passing NoAutoloading lets the
    // daemon keep a binding the user customised in System Settings.
    let applied = kga
        .set_shortcut_keys(&action_id, &keys, SET_PRESENT)
        .context("failed to set the global shortcut")?;

    // Also record our binding as the default, so it appears in
    // System Settings -> Shortcuts and can be restored there.
    kga.set_shortcut_keys(&action_id, &keys, IS_DEFAULT)
        .context("failed to set the default shortcut")?;

    let path = kga
        .get_component(COMPONENT)
        .context("failed to resolve the component object")?;
    let component = ComponentProxyBlocking::builder(&conn)
        .path(path.as_str())
        .context("invalid component object path")?
        .build()
        .context("failed to build the component proxy")?;

    log::info!(
        "global shortcut '{}' registered (active binding: {})",
        config.shortcut,
        describe(&applied)
    );

    let signals = component
        .receive_global_shortcut_pressed()
        .context("failed to subscribe to globalShortcutPressed")?;
    for signal in signals {
        let args = signal.args().context("failed to decode the signal")?;
        if *args.action_unique() == ACTION && sender.send(HotkeyEvent::Toggle).is_err() {
            break;
        }
    }
    Ok(())
}
```

### 1.10 Wiring the signal into the Wayland event loop

The D-Bus listener runs on its own thread and forwards presses through a
`calloop` channel, so the Wayland loop never blocks on D-Bus.

```rust
// Hotkey events, forwarded from the KGlobalAccel thread.
let (tx, channel) = calloop::channel::channel::<HotkeyEvent>();
handle
    .insert_source(channel, |event, _, app| {
        if let calloop::channel::Event::Msg(HotkeyEvent::Toggle) = event {
            app.toggle();
        }
    })
    .map_err(|err| anyhow!("failed to insert the hotkey event source: {err:?}"))?;

hotkey::spawn(tx, app.config.clone())
    .context("failed to start the global-shortcut listener")?;
```

### 1.11 Smaller papercuts

- **Signal argument accessors return `&str`.** Compare with
  `*args.action_unique() == ACTION`, not `args.action_unique() == ACTION`.
- **`calloop::InsertError` does not implement `Error`/`Sync`**, so `.context()`
  will not compile on it. Use
  `.map_err(|err| anyhow!("...: {err:?}"))` instead.

### 1.12 Testing it without touching the session

Because a bad call can take down the compositor, the marshalling is covered by
unit tests that only exercise serialisation — no live bus involved.

```rust
#[test]
fn keys_argument_has_the_a_ai_signature() {
    assert_eq!(<(Vec<i32>,) as Type>::SIGNATURE, "(ai)");
    assert_eq!(<Vec<(Vec<i32>,)> as Type>::SIGNATURE, "a(ai)");
}

/// Regression test for the KWin crash: Qt's QKeySequence demarshaller reads
/// exactly four ints, so the wire form must always carry four.
#[test]
fn keys_round_trip_with_four_ints() {
    let keys: Vec<(Vec<i32>,)> = vec![(encode_sequence(0x1000_0046),)];
    let ctxt = Context::new_dbus(Endian::Little, 0);
    let data = to_bytes(ctxt, &keys).unwrap();
    let (decoded, _): (Vec<(Vec<i32>,)>, usize) = data.deserialize().unwrap();
    assert_eq!(decoded, keys);
    assert_eq!(decoded[0].0.len(), 4);
}
```

---

## Part 2 — Showing the panel with layer-shell

### 2.1 The problem

We need a surface that is:

- anchored to the left or right screen edge,
- full screen height, about 150 px wide,
- drawn **above** normal windows,
- **not** reserving desktop space (it overlays, it is not a dock),
- and able to take keyboard focus while it is open.

A normal `xdg_toplevel` cannot do this: it is a movable, decorated window that
the compositor places wherever it likes.

### 2.2 Why layer-shell

`zwlr_layer_shell_v1` (wlr-layer-shell) is the protocol built for exactly this
class of surface: panels, docks, launchers, notifications, lock screens. A layer
surface is anchored to one or more edges, placed in a layer
(`Background`/`Bottom`/`Top`/`Overlay`), and can declare an *exclusive zone* to
reserve space. KWin implements it.

### 2.3 The toolkit and the boilerplate

`smithay-client-toolkit` (SCTK) wraps the protocol in `LayerShell`,
`LayerSurface`, and the `LayerShellHandler` trait. SCTK's registry machinery
needs three macro invocations to wire our `App` type into its dispatch:

```rust
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
```

### 2.4 Creating and mapping the surface

The panel is created on demand. Anchoring to `LEFT | TOP | BOTTOM` (or
`RIGHT | ...`) makes the surface stretch between the top and bottom edges; a
height of `0` in `set_size` means "fill the anchored span".

```rust
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
}
```

### 2.5 The configure round-trip

A layer surface is not ready to draw after `commit()`. The compositor replies
with a `configure` event carrying the size it actually granted, and only then
may a buffer be attached. For an axis that is anchored on both ends, the
compositor may report `0` — meaning "you fill this axis" — so we fall back to
our configured width.

```rust
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
```

### 2.6 Drawing into an SHM buffer

The MVP renders in software into a shared-memory buffer (`wl_shm`, ARGB8888,
`stride = width * 4`), attaches it, damages the whole surface, and commits.

```rust
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
```

`panel::draw` is plain pixel pushing — a background fill, an accent strip on the
screen edge, a header block, and placeholder list rows:

```rust
pub fn draw(canvas: &mut [u8], width: u32, height: u32, side: Side) {
    let stride = width as usize * 4;

    fill(canvas, stride, 0, 0, width, height, BG);

    // Accent strip along the screen edge.
    let strip = 3;
    let strip_x = match side {
        Side::Left => 0,
        Side::Right => width.saturating_sub(strip),
    };
    fill(canvas, stride, strip_x, 0, strip, height, ACCENT);

    // Header block.
    fill(canvas, stride, 12, 16, width.saturating_sub(24), 28, HEADER);

    // Placeholder list rows.
    let mut y = 64;
    for i in 0..12 {
        if y + 36 > height {
            break;
        }
        let color = if i == 0 { ITEM_HIGHLIGHT } else { ITEM };
        fill(canvas, stride, 12, y, width.saturating_sub(24), 36, color);
        y += 48;
    }
}
```

### 2.7 Hiding

Dropping the `LayerSurface` destroys the underlying `wl_surface`, which unmaps
it. No explicit "hide" request exists in the protocol.

```rust
fn hide(&mut self) {
    if self.panel.take().is_some() {
        log::debug!("panel hidden");
    }
}
```

### 2.8 Pitfall — SCTK's default features need `libxkbcommon-dev`

SCTK's default feature set includes `xkbcommon`, which links against
`libxkbcommon`. On a stock machine without the `-dev` package the build fails
with a missing `libxkbcommon.pc`. Since the MVP does not read keyboard input, we
turn the default features off and keep only what we use:

```toml
smithay-client-toolkit = { version = "0.21", default-features = false, features = ["calloop"] }
```

### 2.9 Pitfall — drawing before `configure`

Attaching a buffer before the compositor has sent `configure` is a protocol
error and the surface will not appear. The `draw()` guard on `panel.height == 0`
exists precisely so that a stray call before the first configure is a no-op.

### 2.10 Pitfall — KWin re-mapping the same layer surface

The tempting optimisation is to keep one surface and just re-map it on each
toggle. In practice KWin has had bugs where a re-mapped layer surface does not
receive a fresh `configure`, leaving it blank. So the MVP creates a **fresh
surface on every show** and destroys it on hide. That costs one configure
round-trip, which is the price of reliability until the pre-mapped approach can
be done carefully.

### 2.11 Keyboard interactivity without trapping the user

`KeyboardInteractivity::Exclusive` makes the compositor focus the panel
automatically, so typing goes to the launcher. The obvious worry is that a
full-height keyboard-grabbing surface could trap the user. It does not: KWin's
`GlobalShortcutFilter::keyboardKey` processes global shortcuts unless
`isKeyboardShortcutsInhibited()` is set, and a layer surface does not set it. So
**Meta+F always dismisses the panel** — you cannot get stuck.

Opening the panel selects the first website (`Ui::select_first`) and expands
the list. **Alt+J** / **Alt+K** move that selection down and up through the
rows still present in `layout` (so clipped overflow rows are skipped), and
**Enter** activates the selected row through the same `App::activate` path as
a click. Repeats of Alt+J/K keep moving; Enter does not retrigger. Shortcut
recording still owns the keyboard while it is active.

(Note: in KWin's layer-shell implementation `acceptsFocus` is a plain bool, so
`Exclusive` and `OnDemand` behave identically there.)

### 2.12 Latency

The single biggest win is architectural: this is a **long-running daemon**, so
there is no process spawn on each keypress. Beyond that:

- The panel is created on demand and destroyed on hide (one `configure`
  round-trip, ~1 frame). This is deliberate, per 2.10.
- The release profile uses LTO + `strip` so the binary loads fast.
- **Next optimisation (not in the MVP):** keep the surface mapped but hidden
  (transparent buffer + empty input region) and "show" it with a single
  `commit`, avoiding the configure round-trip entirely.

### 2.13 The event loop

Wayland events and hotkey events are multiplexed through one `calloop` loop, so
the process is idle (blocked in `poll`) until something happens.

```rust
let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
let handle = event_loop.handle();

// Wayland events. `insert` also dispatches pending events for us.
WaylandSource::new(conn, event_queue)
    .insert(handle.clone())
    .context("failed to insert the Wayland event source")?;

// ... hotkey channel inserted here (see 1.10) ...

event_loop
    .run(None, &mut app, |_| {})
    .context("the event loop terminated with an error")?;
```

---

## Part 3 — Websites: favicons, hit-testing, pre-warmed browser

### 3.1 The problem
The panel must show real content — a "Websites" drop-down whose rows open in
a browser — while keeping the launcher's promise: everything instant, nothing
spawned or mapped on click.

### 3.2 One layout for both pixels and clicks
`panel::layout(width, ui, sites)` computes the clickable `Hit` rects (header
row + one row per site while expanded). The renderer (`panel::draw`) and the
pointer hit-testing (`panel::hit_test`) both consume it, so what is drawn is
exactly what is clickable — they cannot drift apart
(the layout and hit-test share one `hits` slice so they cannot drift apart).

Input arrives through sctk's `SeatHandler` / `PointerHandler`: `get_pointer`
when the seat advertises `Capability::Pointer`, then aggregated
`pointer_frame` events (Enter/Motion/Press/Release). Motion updates a hover
highlight and repaints; `Press` with `BTN_LEFT` (0x110) dispatches to
`App::activate` — toggle the drop-down, or open a site.

### 3.3 Favicons
A background thread GETs each configured site once: the page (512 KiB cap),
scan `<link rel="icon" href="...">` (case-insensitive; the href keeps its
original casing), then fall back to `/favicon.ico`. Raster formats decode via
`image` (PNG/ICO/JPEG/GIF/WebP), SVG via `resvg` (system fonts loaded for SVG
text). Results return over a calloop channel; rows draw a globe placeholder
until their icon arrives.

**Pitfall — byte order.** Wayland `ARGB8888` buffers are *premultiplied* with
bytes `[B, G, R, A]` (little-endian u32); decoders emit straight `[R, G, B,
A]`. Icons are premultiplied on ingest and R/B-swapped when blitted — miss
either and a red favicon renders blue. `render_sample_panel_png` renders the
real panel headlessly to `/tmp/panel_render.png`, which caught exactly this.

### 3.4 Text
`fontdue` rasterises glyphs from a font chosen at startup (common system
paths, then `fc-match`); no system text API is involved at draw time.
Placement pitfall: fontdue's `ymin` is the bitmap's **bottom** edge above the
baseline, so the glyph's top-left is `baseline - ymin - height`.

### 3.5 The browser: a separate, pre-warmed process
At startup the panel spawns `sidepanel --browser` (same binary; GTK runs on
the helper's main thread). The helper builds a `gtk::Window` + wry/WebKitGTK
webview at `about:blank`, calls `realize()`, then `show(); hide();` **before
pumping a single GTK event** — the widget tree (and best effort the WebKit
web process) is warm, yet no Wayland commit ever maps the window: nothing
flashes and no page loads. `delete-event` hides instead of destroying,
keeping the view warm across closes.

Clicks write `"<title>\t<url>\n"` to the helper's stdin; a reader thread
feeds the GTK loop through a channel (so stdin never starves GTK). On a
command the helper `present()`s first — instant visibility — and only then
`load_url`s. The helper exits when stdin closes (panel gone); a crashed
helper is restarted on the next click.

The native WebKitGTK `decide-policy` handler routes clicked HTTP(S) links by
comparing the destination hostname with the current page's hostname. A different
host opens with `xdg-open` on a worker thread, leaving the embedded page intact;
same-host links stay in the view, including links requesting a new window.
Subdomains count as separate hosts. Redirects, forms, reloads, and programmatic
bookmark loads keep WebKit's default policy so initial navigation and sign-in
redirects still work. The worker waits for `xdg-open` to reap it and logs failures.

**Pitfall — why a separate process.** WebKitGTK *aborts* its host process
(glib `G_LOG_LEVEL_ERROR` is always fatal) when its helper binaries are
missing (`…/webkit2gtk-4.1/WebKitNetworkProcess`: "Unable to spawn a new
child process"). This is not hypothetical — it killed the panel on the first
live run. The panel therefore probes for the helpers before spawning and logs
`browser disabled: WebKitGTK runtime not found` instead of ever risking the
daemon; the helper re-checks defensively before touching GTK.

### 3.6 Settings steppers and Meta+F dismissal

The panel's fixed bottom **Settings** button toggles a separate bottom-left
layer surface, offset to the right of a left-anchored sidebar on the same output.
It closes with the sidebar, the footer button, or its close button. A right-anchored
sidebar keeps settings at the bottom-left of the output. The settings surface offers
Light/Dark/System choices, saved as `theme` (default `system`), and renders `− Width +` / `− Height +` rows.
Each stepper row is laid out as two half-hits; either half highlights the
whole row, and the value label is painted exactly once (by the left half).
Clicks step by 40 px within 640–3840 × 400–2160, persist with `Config::save`,
and send `size <w> <h>` to the helper.

**Pitfall — resizing on Wayland.** Clients cannot resize their own toplevels,
so `gtk_window_resize` is a no-op. The helper instead *rebuilds* its
(usually hidden) window at the new size — and only re-presents/reloads the
last URL if the window was visible when the size changed.

`Config::save` is a full TOML rewrite: `browser_width`/`browser_height` are
declared **before** `[[websites]]` because TOML requires scalar values to
precede tables, and `title` skips `None`. `default_config_round_trips` pins
this. Side effect worth knowing: hand-written comments in the config file
are lost once the Settings UI saves.

`theme::spawn` subscribes to XDG Settings portal `SettingChanged` events before
reading `org.freedesktop.appearance/color-scheme` with `ReadAll`. A value of 1
selects dark; other values or an unavailable portal fall back to light. Appearance
updates travel through calloop and repaint both surfaces when System is selected.
Both surfaces share an explicit light/dark palette for text, backgrounds, hover,
selection, dividers, and placeholder icons.

**Meta+F dismisses too.** `App::toggle` calls `Browser::dismiss` (a `hide`
line) before showing or hiding the panel — summoning starts clean, hiding
takes the browser with it. The helper just `window.hide()`s: the view stays
warm, and the helper process only exits when the panel goes away (stdin
EOF) or crashes (then the next click respawns it at the configured size).

### 3.7 Verifying it
- Unit: `keyboard_selection_starts_at_first_and_stops_at_ends`;
  `render_sample_panel_png` renders the
  headless panel (font, icon, hover, ellipsis) to a PNG for eyeballing.
- Live: `ydotool` (over uinput) injects `Super+F` and `BTN_LEFT`, proving
  show → Enter/Motion → `press -> Open(1)` → `panel: opening …` end to end;
  the `pointer_frame` TRACE log shows per-event surface matching.

---

## Summary of the three mechanisms

| | Shortcut | Presentation | Browser |
| --- | --- | --- | --- |
| Protocol | `org.kde.KGlobalAccel` over D-Bus | `zwlr_layer_shell_v1` | stdin lines to a pre-spawned helper process |
| Key type | `QKeySequence` = `(ai)`, **exactly 4 ints** | `LayerSurface` + `wl_shm` buffer | `"<title>\t<url>\n"` |
| The trap | a short array aborts the receiver (KWin) | drawing before `configure` | WebKitGTK aborts its host process |
| Reliability choice | `SetPresent` + autoloading | fresh surface per show | process isolation + probe + respawn |
| Latency strategy | long-running daemon, no spawn | one configure round-trip | window pre-warmed at startup, shown before it loads |

### Floating panel appearance

The default sidebar width is 200 logical pixels. `margin` (default 12),
`border_radius` (default 16), and `opacity` (default 100 percent) are saved
scalar config fields shared by the sidebar and settings surface. Settings
steppers update both surfaces immediately. Layer-shell margins expose the
desktop around the sidebar, while the settings card stays on the same output
and is offset by sidebar width plus two margins on the left. Changing margins
commits new geometry on the existing surfaces. Config loading bounds margin
to 0–80, radius to 0–48, and opacity to 20–100.

After content rendering, `finish_surface` clips rounded corners with subpixel
coverage and scales all BGRA channels together for premultiplied ARGB opacity.
At 100 percent the interior remains opaque; at radius zero corners are square.

### Shortcut recording and live rebinding

SCTK's `xkbcommon` feature supplies keyboard events using the compositor keymap.
The settings shortcut button enters recording mode; modifier-only and repeat
presses are ignored, Escape cancels, and a supported key with Ctrl/Alt/Meta
submits a request. Closing settings, hiding the sidebar, losing keyboard focus,
or losing the keyboard cancels recording. Pending requests disable recording.

The KGlobalAccel listener shares its registering D-Bus connection with a worker
that serializes rebind requests. The worker checks `globalShortcutAvailable`,
snapshots `shortcutKeys`, sets `setShortcutKeys` with SetPresent | NoAutoloading,
and verifies the returned key sequences. A rejected result restores the previous
keys. Each sequence retains the four-int wire format. Successful results travel
through calloop before config persistence; failures display in settings. Startup
uses autoloading and displays the binding returned by KDE. Recording and pending
states suppress the global toggle so capture does not dismiss the sidebar.

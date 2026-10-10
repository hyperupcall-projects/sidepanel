# sidepanel — implementation notes

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

## Summary of the two mechanisms

| | Shortcut | Presentation |
| --- | --- | --- |
| Protocol | `org.kde.KGlobalAccel` over D-Bus | `zwlr_layer_shell_v1` |
| Key type | `QKeySequence` = `(ai)`, **exactly 4 ints** | `LayerSurface` + `wl_shm` buffer |
| The trap | a short array aborts the receiver (KWin) | drawing before `configure` |
| Reliability choice | `SetPresent` + autoloading | fresh surface per show |
| Latency strategy | long-running daemon, no spawn | one configure round-trip |

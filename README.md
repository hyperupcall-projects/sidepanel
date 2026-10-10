# sidepanel

A small, low-latency side-panel launcher for **KDE Plasma on Wayland**.

Press a global shortcut (default **Meta+F**) and a full-height, ~150 px wide
panel appears instantly on the left or right edge of the screen. Press it again
to dismiss.

This is a working MVP of the *mechanism*: global hotkey → instant layer-shell
surface. The panel content is placeholder graphics; wiring up real launcher
entries and text rendering is the next step.

## Why this design

On Wayland, clients cannot grab global keys — the compositor owns the keyboard.
So the app uses KDE's own broker, **KGlobalAccel**:

```
Meta+F ──► KWin (owns the keyboard; hosts the KGlobalAccel daemon)
              │
              └── D-Bus signal `globalShortcutPressed` ──► this app
                                                              │
                                                   create layer surface
                                                              │
                                                   zwlr_layer_shell_v1
```

The window is a **`zwlr_layer_shell_v1`** surface (KWin supports it), anchored
to the left or right edge, full height, `exclusive_zone = 0` so it overlays the
desktop instead of reserving space.

## Requirements

- KDE Plasma on Wayland (KWin provides `zwlr_layer_shell_v1`).
- The KGlobalAccel D-Bus service `org.kde.kglobalaccel`. On Plasma 6 Wayland
  this is hosted **inside KWin** (KWin links `libKGlobalAccelD` and calls
  `KGlobalAccelD::init()`); the standalone `kglobalacceld` daemon is only used by
  other sessions.
- Rust toolchain to build.

No X11 support, by design.

## Build

```sh
cargo build --release
# binary at target/release/sidepanel
```

> Note: `smithay-client-toolkit` is pulled in with `default-features = false`.
> Its default `xkbcommon` feature needs the system `libxkbcommon-dev` package,
> which the MVP does not require. See "Keyboard input" below.

## Run

```sh
./target/release/sidepanel
```

It runs as a daemon (it does not exit). Press **Meta+F** to toggle the panel.

## Install / autostart

Run the installer, which builds the binary, installs it to `~/.local/bin`, and
sets up a **systemd user service** bound to the graphical session:

```sh
./contrib/install.sh
```

That is equivalent to:

- copy `target/release/sidepanel` to `~/.local/bin/sidepanel`,
- copy `contrib/sidepanel.service` to `~/.config/systemd/user/`,
- `systemctl --user enable --now sidepanel.service`.

The service starts with `graphical-session.target` and stops with it. It is
deliberately **not** lingered: the app needs `WAYLAND_DISPLAY`, which only
exists inside a graphical session.

Useful flags: `--prefix DIR` (install elsewhere), `--no-start` (enable but do
not start now), `--uninstall`.

> The installer uses systemd rather than the XDG autostart protocol. On Plasma 6
> the autostart path is backed by systemd anyway, but its generated unit
> hardcodes `Restart=no`, so a crashed daemon would not come back until the next
> login. (`contrib/sidepanel.desktop` is retained for reference but is not used
> by the installer.)

## Configuration

Copy `contrib/config.toml` to `~/.config/sidepanel/config.toml`:

```toml
side = "left"      # "left" or "right"
width = 150        # logical pixels; height always fills the screen
shortcut = "Meta+F"
```

`shortcut` is only the *default*. Once registered, the action also appears in
**System Settings → Shortcuts** (component "Side Panel"), where the user can
rebind it. A user-set binding is respected on later launches.

## Latency notes

The single biggest win is architectural: this is a **long-running daemon**, so
there is no process spawn on each keypress. Beyond that:

- The panel is created on demand and destroyed on hide. That costs one
  `configure` round-trip (~1 frame). This is deliberate: KWin has had bugs
  around re-mapping the *same* layer surface (no fresh `configure`), so creating
  a fresh surface each time is the reliable path.
- The release profile uses LTO + `strip` so the binary loads fast.
- **Next optimization (not in the MVP):** keep the surface mapped but hidden
  (transparent buffer + empty input region) and "show" it with a single
  `commit`, avoiding the configure round-trip entirely.

## Safety

A full-height surface that grabs the keyboard sounds risky, so it is worth being
precise: the panel uses `keyboard_interactivity = Exclusive` so it is focused
automatically, but **global shortcuts still work**. KWin's
`GlobalShortcutFilter::keyboardKey` processes shortcuts unless
`isKeyboardShortcutsInhibited()` is set, which a layer surface does not set.
So **Meta+F always dismisses the panel** — you cannot get stuck.

## Keyboard input (Escape to dismiss)

The MVP does not read keyboard input, so **Escape does not close the panel**;
use Meta+F. Adding Escape requires SCTK's `xkbcommon` feature, which needs the
system dev package:

```sh
# Debian/Ubuntu
sudo apt install libxkbcommon-dev
# Fedora
sudo dnf install libxkbcommon-devel
# Arch
sudo pacman -S libxkbcommon
```

Then in `Cargo.toml`:

```toml
smithay-client-toolkit = { version = "0.21", features = ["calloop", "xkbcommon"] }
```

and re-add the `SeatState` / `SeatHandler` / `KeyboardHandler` wiring (see the
SCTK `simple_layer` example; the `press_key` handler is where you would check
`event.keysym == Keysym::Escape` and call `hide()`).

## Known limitations / next steps

- Placeholder rendering only — no text, no real launcher entries.
- No Escape-to-dismiss (see above).
- No multi-monitor output selection (the compositor picks the active output).
- No fractional-scale handling.
- Pre-mapped surface optimization not yet implemented.

## Troubleshooting

- **"zwlr_layer_shell_v1 is not available"** — you are not on a compositor that
  implements layer-shell (KWin does). Check with `wayland-info | grep layer`.
- **Shortcut never fires** — check that the action appears in
  **System Settings → Shortcuts** (component "Side Panel"). You can introspect
  the service (read-only) with:
  ```sh
  gdbus introspect --session --dest org.kde.kglobalaccel --object-path /kglobalaccel
  ```
- **Shortcut conflicts** — KGlobalAccel resolves clashes itself: `setShortcutKeys`
  returns the binding actually in effect, which the app logs at startup. If
  Meta+F was already taken, the log line shows a different code.

> ⚠️ **Never call a KGlobalAccel method that takes a `QKeySequence` with a bare
> integer.** The argument is a `QKeySequence` (D-Bus `(ai)`: a struct wrapping an
> array of **exactly four** ints). Qt's demarshaller reads all four without
> checking the array length, so a malformed argument makes the receiver read past
> the end and abort on a libdbus assertion. On Plasma 6 Wayland the receiver is
> **KWin**, so a bad call takes down the entire session.

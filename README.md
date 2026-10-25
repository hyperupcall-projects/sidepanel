# sidepanel

A small, low-latency side-panel launcher for **KDE Plasma on Wayland**.

Press a global shortcut (default **Meta+F**) and a frame of **four edge panels
and four corner boxes** appears around the screen. The desktop behind them
dims slightly; the center and gaps let pointer clicks reach the desktop.
Press it again to dismiss.
The first website is highlighted each time the panel opens. Use **Alt+J** to
move down, **Alt+K** to move up, and **Enter** to open the highlighted website.
Navigation stops at the first and last visible entries; holding Alt+J or Alt+K
repeats the movement.

The panel shows a **Websites** section: a drop-down list of the sites from your
config, each row showing the site's favicon on the left. Clicking a row opens
the site in a separate browser window that is **pre-warmed at startup** —
created hidden on `about:blank`, so the window appears immediately on the
first click while loading nothing until then.

Inside that window, links to the current hostname stay in sidepanel. Links to
other hostnames open through `xdg-open` in your default desktop browser (for
example Brave or Chrome), leaving the sidepanel page open. This also applies
to links requesting a new tab or window. Subdomains count as different
hostnames; automatic redirects and sign-in flows continue normally.

Click the **gear in the bottom-left corner box** to open settings just above
and to the right of that box. Click the box again or the settings close button
to dismiss it. The website list remains in the left edge panel; the other edge
panels and corner boxes are empty cards for now.

Settings has independent **Left width**, **Right width**, **Top height**, and
**Bottom height** controls. Each corner box takes its width from the adjacent
left/right panel and its height from the adjacent top/bottom panel. For example,
a left width of 64 and top height of 48 give a 64×48 top-left box. Matching
widths and heights make square boxes. Edge panels stop before the corner boxes,
so none of the eight pieces overlap.

One **Margin** value controls the gap between every neighboring piece and the
inset from the screen edges. Defaults are 64 px for all four edge thicknesses
(giving square corner boxes) and a shared 12 px margin. Slim left panels show
centered website favicons; wider panels also show site labels. Left/right widths range
from 48–800 px and top/bottom heights from 40–400 px. When the screen is too
small, dimensions are fitted proportionally; saved preferences remain unchanged.
The settings card also scales down to stay on screen.

The existing **Light**, **Dark**, and **System** appearance choices, corner
radius, opacity, browser window size, and shortcut recorder are retained.
All changes apply immediately and persist to the config file. Margin (0–80 px),
corner radius (0–48 px), and opacity (20–100%) apply to every frame piece and the
settings card. At 100% opacity the card interiors are fully opaque.
**Desktop dimming** adjusts the black backdrop independently, from 0% (off) to
100% (black) in 5% steps, defaulting to 20%. It updates and saves immediately,
covers the center, gaps, and rounded corners, and disappears with the frame.
Opaque panels retain their normal appearance, and the dimmed gaps remain clickable.
The launcher’s browser windows stay above the dimming, including after moves,
resizes, and reopening. Panels and settings stay above the browser.
System mode follows live desktop changes through the
[XDG Settings portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Settings.html),
using light mode when no preference is available.

Pressing **Meta+F** again dismisses the frame, settings, and any browser window
it opened. The browser is only hidden, so the next bookmark click is just as fast.

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

The frame uses a transparent, output-sized **`zwlr_layer_shell_v1`** overlay,
anchored to all four edges with `exclusive_zone = 0`. Its input region contains
only the eight visible pieces; the empty center and margins pass pointer input
through. Dimming uses a separate, non-interactive top-layer surface below it.
The browser imports that surface through `xdg-foreign` as its transient parent,
so KWin keeps the browser above the dimming. Settings uses another overlay on
the same output. If `xdg-foreign` is unavailable, dimming is disabled.

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

Shortcut capture uses SCTK's `xkbcommon` feature and requires the system
libxkbcommon development package (see "Changing the shortcut" below).

The embedded browser uses WebKitGTK via `wry`, which needs **build-time**
system development packages:

```sh
# Debian/Ubuntu
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libxkbcommon-dev pkg-config
# Fedora
sudo dnf install gtk3-devel webkit2gtk4.1-devel libxkbcommon-devel pkgconf-pkg-config
# Arch
sudo pacman -S gtk3 webkit2gtk-4.1 pkgconf
```

Without them `cargo build` fails in the `webkit2gtk-sys`/`gtk-sys` build
scripts (pkg-config cannot find `webkit2gtk-4.1`/`gtk+-3.0`). At **run** time,
if only the WebKitGTK runtime (`libwebkit2gtk-4.1-0`) is missing, the panel
degrades gracefully: it logs `browser disabled: WebKitGTK runtime not found`
and everything else keeps working.

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
left_width = 64    # logical pixels; 48-800
right_width = 64   # independent of the left width
top_height = 64    # logical pixels; 40-400
bottom_height = 64 # independent of the top height
margin = 12        # 0-80 px shared gap between pieces and screen edges
border_radius = 16 # 0-48 px rounded corners
opacity = 100      # 20-100%; 100 means no transparency
desktop_dim = 20   # 0-100%; 0 disables desktop dimming
shortcut = "Meta+F"
theme = "system"   # "light", "dark", or "system"

# Browser window size (640-3840 x 400-2160); adjustable in the panel's
# Settings UI, which rewrites this file (comments are not preserved).
browser_width = 1280
browser_height = 800

[[websites]]       # the "Websites" drop-down, in order
url = "http://localhost:6005/"
title = "Dev 6005" # optional; defaults to the host, e.g. "localhost:6005"

[[websites]]
url = "http://localhost:6006/"
```

The legacy `width` key is accepted as `left_width`; the obsolete `side` key is
ignored because all four edges are now shown. Saving settings writes the new
keys. When the `[[websites]]` section is omitted, the two localhost entries above
are used. Favicons are fetched in the background — the page's
`<link rel="icon">` first, then `/favicon.ico`; PNG/ICO/JPEG/GIF/WebP and SVG
are all supported — and each row shows a globe placeholder until its icon
arrives. See `contrib/config.toml` for the annotated example.

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
The configured shortcut dismisses the panel except while recording or applying
a replacement. Recording can be cancelled with Escape or its button.

## Changing the shortcut

Open **Settings → Open panels shortcut**, click the current binding, then press
a new combination using **Ctrl**, **Alt**, or **Meta** with a letter, digit,
F1–F12, Space, or navigation key. Shift can be added. Press **Escape** or click
the shortcut button again to cancel. Repeat key events are ignored.

The new shortcut applies immediately and is written to `config.toml` after
KDE accepts it. Conflicting shortcuts are rejected without taking them from
other apps; failed assignments restore the previous binding. No restart is
needed. During recording, the existing global shortcut does not dismiss the
panel. Compositor-reserved shortcuts may be intercepted before reaching the
recorder; cancel and choose another combination in that case.

Registration uses the [KGlobalAccel D-Bus API](https://invent.kde.org/frameworks/kglobalaccel/-/blob/master/src/org.kde.KGlobalAccel.xml).
Startup continues to respect existing KDE shortcut overrides and displays the
binding KDE returns. The default remains **Meta+F**. It can also be changed in
**System Settings → Shortcuts → Side Panel**.

Keyboard capture needs these development packages:

```sh
# Debian/Ubuntu
sudo apt install libxkbcommon-dev
# Fedora
sudo dnf install libxkbcommon-devel
# Arch
sudo pacman -S libxkbcommon
```

Escape cancels shortcut recording; outside recording, use your configured
global shortcut to dismiss the panel.

## Known limitations / next steps

- No Escape-to-dismiss (see above).
- No multi-monitor output selection (the compositor picks the active output).
- No fractional-scale handling.
- Pre-mapped surface optimization not yet implemented.
- The browser window defaults to 1280×800 (adjustable in Settings) and needs
  WebKitGTK (see "Build"); bookmarks reuse the same window.
- The website list does not scroll; only entries that fit in the left panel
  are shown.

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

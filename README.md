# hop

A configurable X11 Alt+Tab window switcher written in Rust, with ARGB
transparency, live window thumbnails, and compositor blur support.

## Features

- **Live thumbnails** — each tile can show a real screenshot of the window
  (via the compositor's backing pixmap), or the app icon, per config
- **App icons** from `_NET_WM_ICON`, with an XDG icon-theme fallback by `WM_CLASS`
- **Window title labels** rendered with Xft (antialiased, Fontconfig fonts)
- **ARGB 32-bit transparency** — semi-transparent tile and popup backgrounds
- **Compositor blur** via the `_KDE_NET_WM_BLUR_BEHIND_REGION` hint (whole popup or
  per-tile) — honored by picom, KWin, and other compositors
- **Styling** — rounded corners, configurable borders, gradients, drop shadows
- **Multi-monitor** — the popup centers on the monitor under the pointer
- **Configurable keybindings** — modifier, next, prev, and cancel keys via TOML
- **Fast** — the popup paints immediately; icons and thumbnails stream in
  progressively without blocking input
- Pure-Rust X11 via `x11rb` (Xft text is the only C dependency)

## Requirements

- An X11 session with EWMH support (tested on XFCE / xfwm4)
- A compositor for transparency and blur. **picom** is recommended; live
  thumbnails require a compositor with the COMPOSITE extension (without one,
  tiles fall back to icons)
- Build-time: a Rust toolchain and the X11 + Xft development libraries
  (e.g. on Arch: `libx11`, `libxft`, `libxrender`, which are usually already
  present in a desktop install)

## Building

```fish
cargo build --release
```

The binary ends up at `target/release/hop`.

## Installing

```fish
install -Dm755 target/release/hop ~/.local/bin/hop
```

## Configuration

hop reads `$XDG_CONFIG_HOME/hop/config.toml` (defaults to
`~/.config/hop/config.toml`). If no config is found, built-in Dracula-themed
defaults are used. Edits take effect the next time the popup opens — no restart
needed.

```fish
mkdir -p ~/.config/hop
cp config.example.toml ~/.config/hop/config.toml
```

[`config.example.toml`](config.example.toml) documents every option. A taste:

```toml
[window]
position           = "center"     # "center" or "x,y"
background         = "#282a36ff"   # popup background (gap/border areas)
border_radius      = 0             # popup corner radius (px)
blur               = false         # blur behind the whole popup

[tile]
width        = 200
height       = 150
content      = "icon"             # "icon" or "thumbnail"
icon_overlay = true               # small corner icon over thumbnails
background   = "#282a36cc"
frame        = "#bd93f9ff"         # selected tile border
inactive     = "#44475aff"         # unselected tile border

[font]
name = "Roboto"
size = 11

[keys]
modifier = "Alt"
next     = "Tab"
prev     = "Shift+Tab"
cancel   = "Escape"
```

Colors accept `#rrggbb` (opaque) or `#rrggbbaa` (with alpha).

## Autostart (XFCE)

Create `~/.config/autostart/hop.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=hop
Exec=/home/<user>/.local/bin/hop
Hidden=false
X-XFCE-Autostart-Override=true
```

Disable xfwm4's built-in Alt+Tab so it doesn't conflict:

```fish
xfconf-query -c xfwm4 -p /general/cycle_windows_key -s ""
```

## Compositor setup

hop draws with an ARGB visual and asks the compositor to blur behind the popup by
setting the standard `_KDE_NET_WM_BLUR_BEHIND_REGION` hint (honored by picom, KWin,
and others). hop **never edits your compositor's config** — it only sets the hint
and its own `WM_CLASS = "hop"`, so you stay in control of your compositor and hop
works with whatever you run. You enable blur (and any shadow/corner rules) yourself,
once.

### picom

Run picom for transparency, blur, and live thumbnails. To enable blur globally:

```
blur-background = true;
blur-method = "dual_kawase";
blur-strength = 8;
```

hop's popup is matchable as `class_g = 'hop'`. Use that to tune picom's effects for
the switcher — for example, to keep blur but drop the shadow and rounded corners on
the popup:

```
shadow-exclude = [ "class_g = 'hop'" ];
rounded-corners-exclude = [ "class_g = 'hop'" ];
```

### Other compositors

Any compositor that honors `_KDE_NET_WM_BLUR_BEHIND_REGION` (e.g. KWin) will blur
behind the popup with no extra setup. Compositors that don't support the hint still
render hop's ARGB transparency; you just won't get blur. Live thumbnails require a
compositor with the COMPOSITE extension.
